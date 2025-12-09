use std::sync::Arc;
use std::time::Duration;

use log::error;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::{mpsc, Mutex};

#[derive(Clone, Debug, Serialize, Type, Event)]
pub struct TlcsConnectionEvent {
    pub status: TlcsConnectionStatus,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Type)]
pub enum TlcsConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, Serialize, Type, Event, Default)]
pub struct TlcsGameEvent {
    pub state: TlcsGameState,
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Serialize, Type, Default)]
pub struct TlcsGameState {
    pub fen: Option<String>,
    pub white_clock_ms: Option<u64>,
    pub black_clock_ms: Option<u64>,
    pub status: Option<String>,
    pub last_move: Option<String>,
    pub can_offer_draw: bool,
    pub can_accept_draw: bool,
    pub can_resign: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TlcsConnectArgs {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub auto_reconnect: bool,
    pub reconnect_interval_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
pub enum TlcsUserAction {
    AcceptOffer,
    OfferDraw,
    Resign,
    DeclineDraw,
    RequestReconnect,
}

enum TlcsControl {
    Send(String),
    Disconnect,
    Reconnect,
}

pub struct TlcsManager {
    handle: Mutex<Option<TlcsHandle>>,
    last_options: Mutex<Option<TlcsConnectArgs>>, 
}

impl Default for TlcsManager {
    fn default() -> Self {
        Self {
            handle: Mutex::new(None),
            last_options: Mutex::new(None),
        }
    }
}

struct TlcsHandle {
    control: mpsc::UnboundedSender<TlcsControl>,
    join: tokio::task::JoinHandle<()>,
}

impl TlcsHandle {
    async fn shutdown(self) {
        let _ = self.control.send(TlcsControl::Disconnect);
        let _ = self.join.await;
    }
}

impl TlcsManager {
    pub async fn connect(&self, options: TlcsConnectArgs, app: AppHandle) {
        {
            let mut last_options = self.last_options.lock().await;
            *last_options = Some(options.clone());
        }

        if let Some(handle) = self.replace_running(None).await {
            handle.shutdown().await;
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let join = tokio::spawn(run_connection(options, app, rx));

        self.replace_running(Some(TlcsHandle { control: tx, join }))
            .await;
    }

    pub async fn disconnect(&self) {
        if let Some(handle) = self.replace_running(None).await {
            handle.shutdown().await;
        }
    }

    pub async fn send_action(&self, action: TlcsUserAction) -> Result<(), String> {
        let handle = self.handle.lock().await;
        let Some(handle) = &*handle else {
            return Err("Not connected".into());
        };

        let payload = match action {
            TlcsUserAction::AcceptOffer => "ACCEPT".to_string(),
            TlcsUserAction::OfferDraw => "DRAW".to_string(),
            TlcsUserAction::Resign => "RESIGN".to_string(),
            TlcsUserAction::DeclineDraw => "DECLINE".to_string(),
            TlcsUserAction::RequestReconnect => {
                handle
                    .control
                    .send(TlcsControl::Reconnect)
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
        };

        handle
            .control
            .send(TlcsControl::Send(payload))
            .map_err(|e| e.to_string())
    }

    pub async fn reconnect(&self, app: AppHandle) -> Result<(), String> {
        let options = {
            let last = self.last_options.lock().await;
            last.clone().ok_or_else(|| "No previous connection".to_string())?
        };
        self.connect(options, app).await;
        Ok(())
    }

    async fn replace_running(&self, next: Option<TlcsHandle>) -> Option<TlcsHandle> {
        let mut handle = self.handle.lock().await;
        std::mem::replace(&mut *handle, next)
    }
}

async fn run_connection(
    options: TlcsConnectArgs,
    app: AppHandle,
    mut control_rx: mpsc::UnboundedReceiver<TlcsControl>,
) {
    let mut opts = options.clone();

    loop {
        emit_status(
            &app,
            TlcsConnectionStatus::Connecting,
            Some("Opening TLCS socket".into()),
        );

        match TcpStream::connect((opts.host.as_str(), opts.port)).await {
            Ok(stream) => {
                emit_status(&app, TlcsConnectionStatus::Connected, None);
                if !handle_stream(stream, &app, &mut control_rx, &opts).await {
                    emit_status(
                        &app,
                        TlcsConnectionStatus::Error,
                        Some("Connection closed".into()),
                    );
                }
            }
            Err(err) => {
                error!("Failed to connect to TLCS server: {err}");
                emit_status(
                    &app,
                    TlcsConnectionStatus::Error,
                    Some(err.to_string()),
                );
            }
        }

        if !opts.auto_reconnect {
            emit_status(
                &app,
                TlcsConnectionStatus::Disconnected,
                Some("Connection stopped".into()),
            );
            break;
        }

        emit_status(
            &app,
            TlcsConnectionStatus::Connecting,
            Some("Reconnecting".into()),
        );
        tokio::time::sleep(Duration::from_millis(opts.reconnect_interval_ms.max(500))).await;
    }
}

async fn handle_stream(
    stream: TcpStream,
    app: &AppHandle,
    control_rx: &mut mpsc::UnboundedReceiver<TlcsControl>,
    options: &TlcsConnectArgs,
) -> bool {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut game_state = TlcsGameState::default();

    if !options.username.is_empty() {
        let login = format!("USER {} {}", options.username, options.password);
        if let Err(err) = writer.write_all(format!("{login}\r\n").as_bytes()).await {
            error!("Failed to send credentials: {err}");
            emit_status(app, TlcsConnectionStatus::Error, Some(err.to_string()));
            return false;
        }
    }

    loop {
        select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        update_state_from_line(&mut game_state, &line);
                        emit_game(app, &game_state, Some(line));
                    }
                    Ok(None) => {
                        return false;
                    }
                    Err(err) => {
                        error!("Failed to read from TLCS stream: {err}");
                        emit_status(app, TlcsConnectionStatus::Error, Some(err.to_string()));
                        return false;
                    }
                }
            }
            control = control_rx.recv() => {
                match control {
                    Some(TlcsControl::Send(cmd)) => {
                        if let Err(err) = writer.write_all(format!("{cmd}\r\n").as_bytes()).await {
                            error!("Failed to send TLCS command: {err}");
                            emit_status(app, TlcsConnectionStatus::Error, Some(err.to_string()));
                            return false;
                        }
                    }
                    Some(TlcsControl::Disconnect) => {
                        emit_status(app, TlcsConnectionStatus::Disconnected, Some("Disconnected by user".into()));
                        return true;
                    }
                    Some(TlcsControl::Reconnect) => {
                        emit_status(app, TlcsConnectionStatus::Connecting, Some("Manual reconnect".into()));
                        return false;
                    }
                    None => return false,
                }
            }
        }
    }
}

fn update_state_from_line(state: &mut TlcsGameState, line: &str) {
    let normalized = line.trim();
    if let Some(fen) = normalized.strip_prefix("fen ") {
        state.fen = Some(fen.trim().to_string());
    }

    if let Some(status) = normalized.strip_prefix("status ") {
        state.status = Some(status.trim().to_string());
    }

    if let Some(last_move) = normalized.strip_prefix("move ") {
        state.last_move = Some(last_move.trim().to_string());
    }

    if let Some(clock_line) = normalized.strip_prefix("clock ") {
        for part in clock_line.split_whitespace() {
            if let Some(value) = part.strip_prefix("w=") {
                if let Ok(ms) = value.parse::<u64>() {
                    state.white_clock_ms = Some(ms);
                }
            }
            if let Some(value) = part.strip_prefix("b=") {
                if let Ok(ms) = value.parse::<u64>() {
                    state.black_clock_ms = Some(ms);
                }
            }
        }
    }

    if normalized.eq_ignore_ascii_case("offer draw") {
        state.can_accept_draw = true;
    }

    if normalized.eq_ignore_ascii_case("offer cancel") {
        state.can_accept_draw = false;
    }

    state.can_offer_draw = true;
    state.can_resign = true;
}

fn emit_status(app: &AppHandle, status: TlcsConnectionStatus, message: Option<String>) {
    let _ = app.emit_all(
        "tlcs-connection",
        TlcsConnectionEvent {
            status,
            message,
        },
    );
}

fn emit_game(app: &AppHandle, state: &TlcsGameState, raw: Option<String>) {
    let _ = app.emit_all(
        "tlcs-game",
        TlcsGameEvent {
            state: state.clone(),
            raw,
        },
    );
}

pub type SharedTlcs = Arc<TlcsManager>;

#[tauri::command]
#[specta::specta]
pub async fn connect_tlcs(
    options: TlcsConnectArgs,
    state: tauri::State<'_, crate::AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    state.tlcs.connect(options, app).await;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn disconnect_tlcs(state: tauri::State<'_, crate::AppState>) -> Result<(), String> {
    state.tlcs.disconnect().await;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn send_tlcs_action(
    action: TlcsUserAction,
    state: tauri::State<'_, crate::AppState>,
) -> Result<(), String> {
    state.tlcs.send_action(action).await
}

#[tauri::command]
#[specta::specta]
pub async fn reconnect_tlcs(
    state: tauri::State<'_, crate::AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    state.tlcs.reconnect(app).await
}

