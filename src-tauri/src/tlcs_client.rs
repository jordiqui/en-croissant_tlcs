use std::{collections::HashSet, sync::Arc, time::Duration};

use log::{error, info, warn};
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpStream},
    sync::{watch, Mutex, RwLock},
    task::JoinHandle,
    time::sleep,
};

use crate::error::Error;
use crate::AppState;

const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;
const MAX_BACKOFF_SECS: u64 = 30;
const MIN_BACKOFF_SECS: u64 = 1;

#[derive(Clone, Debug, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct TlcsStatusEvent {
    pub connected: bool,
    pub address: String,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct TlcsMessageEvent {
    pub game_id: Option<String>,
    pub payload: String,
}

#[derive(Clone, Debug, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct TlcsErrorEvent {
    pub message: String,
}

#[derive(Default)]
pub struct TlcsManager {
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    subscriptions: Arc<RwLock<HashSet<String>>>,
    connection_task: Option<JoinHandle<()>>,
    keep_alive_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    reconnect: bool,
    address: Option<String>,
}

impl TlcsManager {
    pub async fn connect(
        &mut self,
        app_handle: AppHandle,
        host: String,
        port: u16,
        reconnect: bool,
    ) -> Result<(), Error> {
        self.shutdown().await;

        let address = format!("{}:{}", host, port);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        self.address = Some(address.clone());
        self.reconnect = reconnect;
        self.shutdown_tx = Some(shutdown_tx);

        let writer = self.writer.clone();
        let subscriptions = self.subscriptions.clone();

        self.connection_task = Some(tokio::spawn(async move {
            run_connection(
                address,
                app_handle,
                writer,
                subscriptions,
                shutdown_rx,
                reconnect,
            )
            .await;
        }));

        self.start_keep_alive(None, None).await;
        Ok(())
    }

    pub async fn subscribe_game(
        &mut self,
        game_id: String,
        app_handle: AppHandle,
    ) -> Result<(), Error> {
        self.subscriptions.write().await.insert(game_id.clone());
        self.send_frame(format!("SUBSCRIBE {}", game_id).as_str())
            .await
            .map_err(|err| {
                emit_error(&app_handle, &format!("Failed to subscribe: {err}"));
                err
            })
    }

    pub async fn send_move(
        &self,
        game_id: String,
        mv: String,
        app_handle: AppHandle,
    ) -> Result<(), Error> {
        self.send_frame(format!("MOVE {} {}", game_id, mv).as_str())
            .await
            .map_err(|err| {
                emit_error(&app_handle, &format!("Failed to send move: {err}"));
                err
            })
    }

    pub async fn keep_alive(
        &mut self,
        interval_secs: Option<u64>,
        payload: Option<String>,
    ) -> Result<(), Error> {
        self.start_keep_alive(interval_secs, payload).await;
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<(), Error> {
        self.reconnect = false;
        self.shutdown().await;
        Ok(())
    }

    async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.keep_alive_task.take() {
            handle.abort();
        }

        if let Some(handle) = self.connection_task.take() {
            let _ = handle.await;
        }

        self.writer.lock().await.take();
        self.address = None;
    }

    async fn send_frame(&self, message: &str) -> Result<(), Error> {
        let mut guard = self.writer.lock().await;
        let writer = guard.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No active TLCS connection",
            )
        })?;

        let mut framed = message.to_string();
        if !framed.ends_with("\r\n") {
            framed.push_str("\r\n");
        }

        writer.write_all(framed.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn start_keep_alive(&mut self, interval_secs: Option<u64>, payload: Option<String>) {
        if let Some(handle) = self.keep_alive_task.take() {
            handle.abort();
        }

        let writer = self.writer.clone();
        let interval = interval_secs.unwrap_or(DEFAULT_KEEP_ALIVE_SECS);
        let message = payload.unwrap_or_else(|| "PING".to_string());
        let mut shutdown_rx = self
            .shutdown_tx
            .as_ref()
            .map(|tx| tx.subscribe())
            .unwrap_or_else(|| watch::channel(false).1);

        self.keep_alive_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = sleep(Duration::from_secs(interval)) => {
                        if let Err(err) = send_keep_alive(writer.clone(), &message).await {
                            warn!("Keep-alive send failed: {}", err);
                        }
                    }
                }
            }
        }));
    }
}

async fn run_connection(
    address: String,
    app_handle: AppHandle,
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    subscriptions: Arc<RwLock<HashSet<String>>>,
    mut shutdown_rx: watch::Receiver<bool>,
    reconnect: bool,
) {
    let mut backoff = Duration::from_secs(MIN_BACKOFF_SECS);

    loop {
        let connect_future = TcpStream::connect(&address);
        let stream = tokio::select! {
            _ = shutdown_rx.changed() => {
                break;
            }
            result = connect_future => result
        };

        let stream = match stream {
            Ok(stream) => {
                info!("Connected to TLCS server at {}", address);
                let _ = app_handle.emit_all(
                    "tlcs://status",
                    TlcsStatusEvent {
                        connected: true,
                        address: address.clone(),
                        message: Some("connected".to_string()),
                    },
                );
                backoff = Duration::from_secs(MIN_BACKOFF_SECS);
                stream
            }
            Err(err) => {
                emit_error(
                    &app_handle,
                    &format!("Connection to {} failed: {}", address, err),
                );
                if !reconnect {
                    break;
                }
                wait_with_backoff(&mut shutdown_rx, backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(MAX_BACKOFF_SECS));
                continue;
            }
        };

        let (read_half, write_half) = stream.into_split();
        writer.lock().await.replace(write_half);

        if let Err(err) = resend_subscriptions(&writer, &subscriptions).await {
            emit_error(
                &app_handle,
                &format!("Failed to restore subscriptions: {err}"),
            );
        }

        let mut reader = BufReader::new(read_half);
        let mut buffer = Vec::new();

        loop {
            buffer.clear();
            let read_result = tokio::select! {
                _ = shutdown_rx.changed() => {
                    break;
                }
                result = reader.read_until(b'\n', &mut buffer) => result
            };

            match read_result {
                Ok(0) => {
                    warn!("TLCS connection closed by remote host");
                    break;
                }
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buffer)
                        .trim_end_matches(['\r', '\n'])
                        .to_string();
                    handle_incoming_line(&app_handle, line);
                }
                Err(err) => {
                    emit_error(&app_handle, &format!("Failed to read from TLCS: {err}"));
                    break;
                }
            }
        }

        writer.lock().await.take();
        let _ = app_handle.emit_all(
            "tlcs://status",
            TlcsStatusEvent {
                connected: false,
                address: address.clone(),
                message: Some("disconnected".to_string()),
            },
        );

        if !reconnect {
            break;
        }

        wait_with_backoff(&mut shutdown_rx, backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(MAX_BACKOFF_SECS));
    }

    writer.lock().await.take();
    let _ = app_handle.emit_all(
        "tlcs://status",
        TlcsStatusEvent {
            connected: false,
            address,
            message: Some("stopped".to_string()),
        },
    );
}

fn handle_incoming_line(app_handle: &AppHandle, line: String) {
    if line.trim().is_empty() {
        return;
    }

    if let Some(rest) = line.strip_prefix("MOVE ") {
        let mut segments = rest.splitn(2, ' ');
        let game_id = segments.next().map(|s| s.to_string());
        let payload = segments.next().unwrap_or("").to_string();
        let _ = app_handle.emit_all("tlcs://move", TlcsMessageEvent { game_id, payload });
    } else {
        let _ = app_handle.emit_all(
            "tlcs://message",
            TlcsMessageEvent {
                game_id: None,
                payload: line,
            },
        );
    }
}

async fn resend_subscriptions(
    writer: &Arc<Mutex<Option<OwnedWriteHalf>>>,
    subscriptions: &Arc<RwLock<HashSet<String>>>,
) -> Result<(), Error> {
    let subs = subscriptions.read().await.clone();
    for sub in subs {
        send_keep_alive(writer.clone(), format!("SUBSCRIBE {}", sub).as_str()).await?;
    }
    Ok(())
}

async fn send_keep_alive(
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    message: &str,
) -> Result<(), Error> {
    let mut guard = writer.lock().await;
    if let Some(writer) = guard.as_mut() {
        let mut payload = message.to_string();
        if !payload.ends_with("\r\n") {
            payload.push_str("\r\n");
        }
        writer.write_all(payload.as_bytes()).await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn wait_with_backoff(shutdown_rx: &mut watch::Receiver<bool>, backoff: Duration) {
    let sleep_future = sleep(backoff);
    tokio::select! {
        _ = shutdown_rx.changed() => {}
        _ = sleep_future => {}
    }
}

fn emit_error(app_handle: &AppHandle, message: &str) {
    error!("{}", message);
    let _ = app_handle.emit_all(
        "tlcs://error",
        TlcsErrorEvent {
            message: message.to_string(),
        },
    );
}

#[tauri::command]
#[specta::specta]
pub async fn connect(
    host: String,
    port: u16,
    reconnect: bool,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), Error> {
    let mut manager = state.tlcs_client.write().await;
    manager.connect(app_handle, host, port, reconnect).await
}

#[tauri::command]
#[specta::specta]
pub async fn subscribe_game(
    game_id: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), Error> {
    let mut manager = state.tlcs_client.write().await;
    manager.subscribe_game(game_id, app_handle).await
}

#[tauri::command]
#[specta::specta]
pub async fn send_move(
    game_id: String,
    mv: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), Error> {
    let manager = state.tlcs_client.read().await;
    manager.send_move(game_id, mv, app_handle).await
}

#[tauri::command]
#[specta::specta]
pub async fn keep_alive(
    interval_secs: Option<u64>,
    payload: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<(), Error> {
    let mut manager = state.tlcs_client.write().await;
    manager.keep_alive(interval_secs, payload).await
}

#[tauri::command]
#[specta::specta]
pub async fn disconnect(state: tauri::State<'_, AppState>) -> Result<(), Error> {
    let mut manager = state.tlcs_client.write().await;
    manager.disconnect().await
}
