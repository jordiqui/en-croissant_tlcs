use std::collections::HashMap;
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use log::error;
use serde::{Deserialize, Serialize};
use shakmaty::{fen::Fen, san::SanPlus, uci::UciMove, CastlingMode, Chess, EnPassantMode};
use specta::Type;
use tauri::{path::BaseDirectory, AppHandle};
use tauri_specta::Event;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::{mpsc, watch, Mutex, RwLock};

use crate::chess::AnalysisOptions;
use crate::error::Error;
use crate::AppState;

const DEFAULT_ROTATION_BYTES: u64 = 512 * 1024;
const DEFAULT_ROTATION_FILES: usize = 5;

#[derive(Clone)]
struct RotatingLog {
    inner: Arc<RotatingLogInner>,
}

struct RotatingLogInner {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
}

impl RotatingLog {
    fn new(path: PathBuf, max_bytes: u64, max_files: usize) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }

        Ok(Self {
            inner: Arc::new(RotatingLogInner {
                path,
                max_bytes,
                max_files,
            }),
        })
    }

    fn info(&self, message: &str) {
        let _ = self.write("INFO", message);
    }

    fn debug(&self, message: &str) {
        let _ = self.write("DEBUG", message);
    }

    fn error(&self, message: &str) {
        let _ = self.write("ERROR", message);
    }

    fn write(&self, level: &str, message: &str) -> Result<(), Error> {
        self.rotate_if_needed()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.path)?;
        let now = Utc::now().to_rfc3339();
        writeln!(file, "[{now}][{level}] {message}")?;
        Ok(())
    }

    fn rotate_if_needed(&self) -> Result<(), Error> {
        let metadata = std::fs::metadata(&self.inner.path);
        if metadata.is_err() {
            return Ok(());
        }
        let metadata = metadata?;
        if metadata.len() < self.inner.max_bytes {
            return Ok(());
        }

        for index in (1..self.inner.max_files).rev() {
            let from = self.inner.path.with_extension(format!("log.{index}"));
            let to = self.inner.path.with_extension(format!("log.{}", index + 1));
            if from.exists() {
                let _ = std::fs::rename(&from, to);
            }
        }

        let rotated = self.inner.path.with_extension("log.1");
        let _ = std::fs::rename(&self.inner.path, rotated);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct TlcsStatus {
    pub recording: bool,
    pub pgn_path: Option<String>,
    pub moves_recorded: usize,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TlcsConnectOptions {
    pub host: String,
    pub port: u16,
    pub event: Option<String>,
    pub site: Option<String>,
    pub white: Option<String>,
    pub black: Option<String>,
    pub initial_fen: Option<String>,
    pub pgn_path: Option<String>,
}

struct TlcsRecorder {
    writer: BufWriter<File>,
    position: Chess,
    moves: Vec<String>,
    start_fen: String,
    result: Option<String>,
    log: RotatingLog,
    pgn_path: PathBuf,
}

impl TlcsRecorder {
    fn new(
        pgn_path: PathBuf,
        options: &TlcsConnectOptions,
        log: RotatingLog,
    ) -> Result<Self, Error> {
        if let Some(parent) = pgn_path.parent() {
            create_dir_all(parent)?;
        }

        let mut writer = BufWriter::new(File::create(&pgn_path)?);

        let position = if let Some(fen) = &options.initial_fen {
            let fen: Fen = fen.parse()?;
            fen.into_position(CastlingMode::Chess960)?
        } else {
            Chess::default()
        };

        let mut headers: HashMap<&str, String> = HashMap::new();
        headers.insert(
            "Event",
            options.event.clone().unwrap_or_else(|| "TLCS Live".into()),
        );
        headers.insert(
            "Site",
            options.site.clone().unwrap_or_else(|| "TLCS".into()),
        );
        headers.insert("Date", Utc::now().format("%Y.%m.%d").to_string());
        headers.insert(
            "White",
            options.white.clone().unwrap_or_else(|| "Unknown".into()),
        );
        headers.insert(
            "Black",
            options.black.clone().unwrap_or_else(|| "Unknown".into()),
        );
        headers.insert("Round", "1".into());
        headers.insert("Result", "*".into());

        for (key, value) in &headers {
            writeln!(writer, "[{key} \"{value}\"]")?;
        }

        if options.initial_fen.is_some() {
            writeln!(writer, "[SetUp \"1\"]")?;
            writeln!(
                writer,
                "[FEN \"{}\"]",
                options.initial_fen.as_ref().unwrap()
            )?;
        }

        writeln!(writer)?;

        Ok(Self {
            writer,
            position,
            moves: Vec::new(),
            start_fen: options.initial_fen.clone().unwrap_or_else(|| {
                Fen::from_position(Chess::default(), EnPassantMode::Legal).to_string()
            }),
            result: None,
            log,
            pgn_path,
        })
    }

    fn pgn_path(&self) -> PathBuf {
        self.pgn_path.clone()
    }

    fn moves_recorded(&self) -> usize {
        self.moves.len()
    }

    fn finish(&mut self, outcome: &str) -> Result<(), Error> {
        if self.result.is_some() {
            return Ok(());
        }
        self.result = Some(outcome.to_string());
        write!(self.writer, "{outcome}\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn append_moves_from_line(&mut self, line: &str) -> Result<(), Error> {
        for token in Self::tokens_from_line(line) {
            self.append_token(&token)?;
        }
        Ok(())
    }

    fn tokens_from_line(line: &str) -> Vec<String> {
        line.split_whitespace()
            .flat_map(|token| token.split('.'))
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
            .map(|t| t.to_string())
            .collect()
    }

    fn append_token(&mut self, token: &str) -> Result<(), Error> {
        if token.is_empty() {
            return Ok(());
        }

        if matches!(token, "1-0" | "0-1" | "1/2-1/2" | "*") {
            self.finish(token)?;
            return Ok(());
        }

        if let Ok(san) = SanPlus::from_ascii(token.as_bytes()) {
            let current_position = self.position.clone();
            let mv = san.to_move(&self.position)?;
            let uci = UciMove::from_move(&mv, &current_position);
            self.write_san(&san.to_string())?;
            self.position.play_unchecked(&mv);
            self.moves.push(uci.to_string());
            return Ok(());
        }

        if let Ok(uci) = UciMove::from_ascii(token.as_bytes()) {
            let mv = uci.to_move(&self.position)?;
            let san = SanPlus::from_move(&self.position, &mv);
            self.write_san(&san.to_string())?;
            self.position.play_unchecked(&mv);
            self.moves.push(uci.to_string());
        }

        Ok(())
    }

    fn write_san(&mut self, san: &str) -> Result<(), Error> {
        let ply = self.moves.len();
        let move_number = (ply / 2) + 1;
        if ply % 2 == 0 {
            write!(self.writer, "{move_number}. {san} ")?;
        } else {
            write!(self.writer, "{san} ")?;
        }
        self.writer.flush()?;
        Ok(())
    }

    fn analysis_options(&self) -> AnalysisOptions {
        AnalysisOptions {
            fen: self.start_fen.clone(),
            moves: self.moves.clone(),
            annotate_novelties: false,
            reference_db: None,
            reversed: false,
        }
    }
}

pub struct TlcsHandle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    recorder: Arc<RwLock<TlcsRecorder>>,
    log: RotatingLog,
}

impl TlcsHandle {
    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

#[tauri::command]
#[specta::specta]
pub async fn start_tlcs_stream(
    options: TlcsConnectOptions,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, Error> {
    let tlcs_dir = app.path().resolve("tlcs", BaseDirectory::AppData)?;
    create_dir_all(&tlcs_dir)?;

    let pgn_path = options
        .pgn_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            tlcs_dir.join(format!("tlcs-{}.pgn", Utc::now().format("%Y%m%dT%H%M%SZ")))
        });

    let log_path = tlcs_dir.join("tlcs.log");
    let log = RotatingLog::new(log_path, DEFAULT_ROTATION_BYTES, DEFAULT_ROTATION_FILES)?;
    log.info(&format!(
        "Starting TLCS stream {}:{} -> {}",
        options.host,
        options.port,
        pgn_path.to_string_lossy()
    ));

    let recorder = Arc::new(RwLock::new(TlcsRecorder::new(
        pgn_path.clone(),
        &options,
        log.clone(),
    )?));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let mut guard = state.tlcs_handle.write().await;

    if let Some(handle) = guard.take() {
        log.info("Stopping existing TLCS session before starting new one");
        handle.stop().await;
    }

    let host = options.host.clone();
    let port = options.port;
    let log_clone = log.clone();
    let recorder_clone = recorder.clone();

    let task = tokio::spawn(async move {
        match TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => {
                log_clone.info("Connected to TLCS server");
                let mut reader = BufReader::new(stream).lines();

                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            log_clone.info("TLCS stream stop requested");
                            break;
                        }
                        line = reader.next_line() => {
                            match line {
                                Ok(Some(l)) => {
                                    log_clone.debug(&format!("RX: {}", l));
                                    let mut recorder = recorder_clone.write().await;
                                    if let Err(err) = recorder.append_moves_from_line(&l) {
                                        log_clone.error(&format!("Failed to parse TLCS line: {err}"));
                                    }
                                }
                                Ok(None) => {
                                    log_clone.info("TLCS stream closed by server");
                                    break;
                                }
                                Err(err) => {
                                    log_clone.error(&format!("TLCS stream read error: {err}"));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(err) => {
                log_clone.error(&format!("Unable to connect to TLCS server: {err}"));
            }
        }
    });

    *guard = Some(TlcsHandle {
        shutdown,
        task,
        recorder,
        log,
    });

    Ok(pgn_path.to_string_lossy().to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn stop_tlcs_stream(state: tauri::State<'_, AppState>) -> Result<Option<String>, Error> {
    let mut guard = state.tlcs_handle.write().await;
    if let Some(handle) = guard.take() {
        let path = {
            let recorder = handle.recorder.read().await;
            recorder.pgn_path()
        };
        handle.log.info("Stopping TLCS stream");
        handle.stop().await;
        return Ok(Some(path.to_string_lossy().to_string()));
    }
    Ok(None)
}

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
    handle: Mutex<Option<TlcsConnectionHandle>>,
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

struct TlcsConnectionHandle {
    control: mpsc::UnboundedSender<TlcsControl>,
    join: tokio::task::JoinHandle<()>,
}

impl TlcsConnectionHandle {
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

        self.replace_running(Some(TlcsConnectionHandle { control: tx, join }))
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
            last.clone()
                .ok_or_else(|| "No previous connection".to_string())?
        };
        self.connect(options, app).await;
        Ok(())
    }

    async fn replace_running(
        &self,
        next: Option<TlcsConnectionHandle>,
    ) -> Option<TlcsConnectionHandle> {
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
                emit_status(&app, TlcsConnectionStatus::Error, Some(err.to_string()));
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
    let _ = app.emit_all("tlcs-connection", TlcsConnectionEvent { status, message });
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

#[tauri::command]
#[specta::specta]
pub async fn tlcs_status(state: tauri::State<'_, AppState>) -> Result<TlcsStatus, Error> {
    let guard = state.tlcs_handle.read().await;
    if let Some(handle) = guard.as_ref() {
        let recorder = handle.recorder.read().await;
        return Ok(TlcsStatus {
            recording: true,
            pgn_path: Some(recorder.pgn_path().to_string_lossy().to_string()),
            moves_recorded: recorder.moves_recorded(),
        });
    }

    Ok(TlcsStatus {
        recording: false,
        pgn_path: None,
        moves_recorded: 0,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn tlcs_analysis_options(
    state: tauri::State<'_, AppState>,
) -> Result<Option<AnalysisOptions>, Error> {
    let guard = state.tlcs_handle.read().await;
    if let Some(handle) = guard.as_ref() {
        let recorder = handle.recorder.read().await;
        return Ok(Some(recorder.analysis_options()));
    }
    Ok(None)
}
