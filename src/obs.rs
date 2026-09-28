//! OBS WebSocket v5 client. Auto-connects to the local OBS Studio and
//! exposes:
//!   * status (connected / version / last error)
//!   * ONE shared command handle (`AppState::obs_cmd_tx`) that is republished on
//!     every connect and withdrawn on every disconnect, so a caller either gets
//!     a sender that really reaches OBS or a clear "not connected" (`None`);
//!   * automatic reconnection: when *either* half of a connection ends (OBS
//!     closed the socket, OBS restarted, the command channel closed) the other
//!     half is cancelled and the connection attempt returns, so the reconnect
//!     loop runs again (P1-05);
//!   * mirroring of the subtitled line into an OBS text source when
//!     `overlay.mirror_to_text_source` is enabled (see [`MIRROR_INPUT_NAME`]);
//!   * automatic creation of a hidden `browser_source` named
//!     "StreamLiveTranslateAdmin" that hosts the admin panel, so the user
//!     can open it as an in-OBS panel via OpenInputInteract (no external
//!     browser window required after the very first launch).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::ObsConfig;
use crate::subtitle::SubtitleEvent;
use crate::AppState;

/// The OBS input (browser source) name we auto-create to host the admin
/// panel inside OBS. Users can find it under Sources and use the
/// "Interact" button to manage the plugin without leaving OBS.
pub const DOCK_INPUT_NAME: &str = "StreamLiveTranslateAdmin";

/// Name of the OBS **text source** the subtitle mirror writes to.
///
/// This is the only input name the mirror ever touches, and it is deliberately
/// the historical name: installs that already created a text source called
/// "Subtitles" (as the docs/panel told them to) keep working, and a source with
/// that name is created lazily the first time a line is mirrored (see
/// [`ws_set_text`]). The old code was confusing here — it *updated* whatever
/// name the caller passed but *created* a differently named fallback source.
pub const MIRROR_INPUT_NAME: &str = "Subtitles";

/// OBS 的文本源在不同平台根本不是同一个 input kind：
///   * Windows -> GDI+ (`text_gdiplus_v2`)
///   * Linux / macOS -> FreeType (`text_ft2_source`)
/// 写死 Windows 的 kind，Linux 上的 CreateInput 会被 OBS 直接拒绝。
#[cfg(target_os = "windows")]
const OBS_TEXT_KIND: &str = "text_gdiplus_v2";
#[cfg(not(target_os = "windows"))]
const OBS_TEXT_KIND: &str = "text_ft2_source";

/// 兜底字体。Linux 上没有 Microsoft YaHei，写死它会让文本源回退到
/// 默认字体甚至渲染成方块，所以各平台挑一个几乎必定存在的中文字体。
#[cfg(target_os = "windows")]
const OBS_TEXT_FONT: &str = "Microsoft YaHei";
#[cfg(target_os = "macos")]
const OBS_TEXT_FONT: &str = "PingFang SC";
#[cfg(target_os = "linux")]
const OBS_TEXT_FONT: &str = "Noto Sans CJK SC";

/// Upper bound on how long `try_connect` may take to clean up after one of its
/// two halves ended. Aborting a task is immediate, but we never *trust* that a
/// half will notice: the reconnect loop must resume within this window even if
/// a half is wedged in a write. This is what makes the admin panel's OBS dot
/// recover on its own instead of staying red until the process is restarted.
const DISCONNECT_GRACE: Duration = Duration::from_millis(500);

/// Command channel sender type, so the shared handle's type is written once.
type CmdTx = mpsc::Sender<ObsCommand>;

#[derive(Debug, Clone, Default)]
pub struct ObsStatus {
    pub connected: bool,
    pub version: Option<String>,
    pub last_error: Option<String>,
}

/// Why a command could not be handed to OBS. A command that is not accepted
/// must never look like success (P1-05), so senders return this instead of
/// swallowing the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendCommandError {
    /// No live connection: `obs.auto_connect` is off, OBS is unreachable, or
    /// the connection just died.
    NotConnected,
    /// The connection's command queue is full. The command is dropped on
    /// purpose — blocking here would delay subtitle delivery.
    QueueFull,
}

impl std::fmt::Display for SendCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendCommandError::NotConnected => write!(f, "OBS is not connected"),
            SendCommandError::QueueFull => write!(f, "OBS command queue is full"),
        }
    }
}

impl std::error::Error for SendCommandError {}

pub struct ObsClient {
    cfg: ObsConfig,
    status: Arc<Mutex<ObsStatus>>,
    /// Live command channel of the *current* connection. Written only by
    /// `ObsShared::set_cmd_tx` (single writer), which also updates
    /// `AppState::obs_cmd_tx` in the very same call so the two can never
    /// disagree about which connection is alive.
    cmd_tx: Arc<Mutex<Option<CmdTx>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Last known admin URL (host:port). Used to rebuild the browser
    /// source after reconnects.
    admin_url: Arc<Mutex<Option<String>>>,
    /// Set by [`ObsClient::stop`]: the reconnect loop exits instead of dialing
    /// OBS again. Without this, cancelling the connection would turn a stop
    /// request into an immediate reconnect.
    stopping: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub enum ObsCommand {
    UpdateTextSource {
        name: String,
        text: String,
    },
    BroadcastEvent {
        event: String,
        data: serde_json::Value,
    },
    /// Open the admin panel as an interactive panel inside OBS.
    OpenAdminInObs,
    Shutdown,
}

impl ObsClient {
    pub fn new(cfg: ObsConfig) -> Self {
        Self {
            cfg,
            status: Arc::new(Mutex::new(ObsStatus::default())),
            cmd_tx: Arc::new(Mutex::new(None)),
            task: None,
            admin_url: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn status(&self) -> ObsStatus {
        self.status.lock().clone()
    }

    /// The command handle for the connection that is alive **right now**, or
    /// `None` when OBS is not connected.
    ///
    /// Callers must not cache the result across a reconnect: the returned
    /// sender belongs to one connection only. A handle whose receiver is
    /// already gone is withdrawn here instead of being handed out, so a stale
    /// handle can never be mistaken for a working one.
    pub fn sender(&self) -> Option<CmdTx> {
        if !self.status.lock().connected {
            return None;
        }
        let tx = self.cmd_tx.lock().clone();
        match tx {
            Some(tx) if tx.is_closed() => {
                *self.cmd_tx.lock() = None;
                self.status.lock().connected = false;
                None
            }
            other => other,
        }
    }

    /// Fire-and-forget command delivery that never reports a dropped command
    /// as success.
    pub fn try_send(&self, cmd: ObsCommand) -> std::result::Result<(), SendCommandError> {
        if !self.status.lock().connected {
            return Err(SendCommandError::NotConnected);
        }
        let sent = try_send_command(&self.cmd_tx, cmd);
        if sent.is_err() {
            self.status.lock().connected = false;
        }
        sent
    }

    /// Record the URL the admin panel is being served from, so the OBS
    /// client can rebuild the in-OBS browser source after a reconnect.
    pub fn set_admin_url(&self, url: String) {
        *self.admin_url.lock() = Some(url);
    }

    pub fn stop(&mut self) {
        // Stop for good: end the current connection *and* keep the reconnect
        // loop from dialing again.
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(tx) = self.cmd_tx.lock().take() {
            let _ = tx.try_send(ObsCommand::Shutdown);
        }
        self.status.lock().connected = false;
    }
}

/// The places a live connection publishes its shared state into.
///
/// Extracted from `AppState` so the connect / disconnect / reconnect logic can
/// be exercised by unit tests against a fake OBS on loopback without building a
/// whole `AppState` (which owns recording, pipeline, tokens, ...).
trait ObsShared: Send + Sync {
    /// Publish (`Some`) or withdraw (`None`) the one shared command handle.
    fn set_cmd_tx(&self, tx: Option<CmdTx>);
    fn set_connected(&self, version: Option<String>);
    /// Remember a non-fatal error (e.g. a failed CreateInput) without changing
    /// the connection state.
    fn set_last_error(&self, err: Option<String>);
    /// Mark the connection gone. `Some(reason)` is shown to the user; `None`
    /// keeps the previous reason (an already-explained close).
    fn set_disconnected(&self, err: Option<String>);
}

/// `ObsShared` for the real engine: writes into `AppState` (what the admin
/// panel and every other module see) *and* into the `ObsClient` cells (what
/// `ObsClient::status()` / `sender()` read). Both are written in the same call
/// so they can never drift apart across a reconnect (defect P1-05 #2).
struct EngineShared {
    state: Arc<AppState>,
    client_cmd_tx: Arc<Mutex<Option<CmdTx>>>,
    client_status: Arc<Mutex<ObsStatus>>,
}

impl ObsShared for EngineShared {
    fn set_cmd_tx(&self, tx: Option<CmdTx>) {
        // Single writer for both handles. `state.obs_cmd_tx` is what callers
        // send through; the client copy only backs `ObsClient::sender()`.
        *self.client_cmd_tx.lock() = tx.clone();
        *self.state.obs_cmd_tx.lock() = tx;
    }

    fn set_connected(&self, version: Option<String>) {
        {
            let mut s = self.client_status.lock();
            s.connected = true;
            s.version = version;
            s.last_error = None;
        }
        let mut st = self.state.status.write();
        st.obs_connected = true;
        st.obs_error = None;
    }

    fn set_last_error(&self, err: Option<String>) {
        // Only the client cell: a failed request is not a connection failure and
        // must not light the panel's "OBS error" up while we are still online.
        self.client_status.lock().last_error = err;
    }

    fn set_disconnected(&self, err: Option<String>) {
        {
            let mut s = self.client_status.lock();
            s.connected = false;
            if err.is_some() {
                s.last_error = err.clone();
            }
        }
        let mut st = self.state.status.write();
        st.obs_connected = false;
        if let Some(e) = err {
            st.obs_error = Some(e);
        }
    }
}

pub fn spawn(state: Arc<AppState>) -> Arc<Mutex<ObsClient>> {
    let cfg = state.config.read().obs.clone();
    let client = Arc::new(Mutex::new(ObsClient::new(cfg)));
    let inner = client.clone();
    let loop_state = state.clone();
    tokio::spawn(async move {
        run_loop(loop_state, inner).await;
    });
    // Subtitle -> OBS text source mirror. Own task, so nothing here can ever
    // delay subtitle delivery.
    tokio::spawn(mirror_pump(state));
    client
}

async fn run_loop(state: Arc<AppState>, client: Arc<Mutex<ObsClient>>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if client.lock().stopping.load(Ordering::Relaxed) {
            info!("OBS client stopped; no further reconnects");
            break;
        }
        let cfg = state.config.read().obs.clone();
        if !cfg.auto_connect {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let shared: Arc<dyn ObsShared> = {
            let c = client.lock();
            Arc::new(EngineShared {
                state: state.clone(),
                client_cmd_tx: c.cmd_tx.clone(),
                client_status: c.status.clone(),
            })
        };
        let admin_url = client.lock().admin_url.lock().clone();
        match try_connect(&cfg, &shared, admin_url).await {
            Err(e) => {
                // Full cause chain, so the admin panel and the log both show why
                // (DNS, refused, bad password) instead of just the outer context.
                warn!(error = %format!("{e:#}"), "OBS WebSocket connect failed");
                shared.set_disconnected(Some(format!("{e:#}")));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Ok(()) => {
                // The connection ended (OBS closed / restarted). Try again at
                // once with a fresh backoff.
                backoff = Duration::from_secs(2);
            }
        }
    }
}

/// Mirror the line the overlay is showing into the OBS text source, if the user
/// turned `overlay.mirror_to_text_source` on.
///
/// Fire and forget: decisions are made per event, delivery is a `try_send` on
/// the shared handle, and a failure only logs — it never blocks this task, let
/// alone the subtitle pipeline.
async fn mirror_pump(state: Arc<AppState>) {
    let mut events = state.subtitle.subscribe();
    loop {
        match events.recv().await {
            Ok(ev) => {
                let enabled = state.config.read().overlay.mirror_to_text_source;
                let _ = mirror_one(enabled, &ev, &state.obs_cmd_tx);
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "subtitle mirror fell behind; some lines were not mirrored to OBS"
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// One mirror step: decide (see [`mirror_command`]) and deliver.
///
/// Returns whether a command was produced *and* accepted by a live connection.
/// Kept separate from the pump so tests can drive it with a plain event and a
/// command slot, with or without a socket.
fn mirror_one(enabled: bool, ev: &SubtitleEvent, slot: &Mutex<Option<CmdTx>>) -> bool {
    match mirror_command(enabled, ev) {
        None => false,
        Some(cmd) => match try_send_command(slot, cmd) {
            Ok(()) => true,
            Err(e) => {
                debug!(error = %e, "OBS text-source mirror command dropped");
                false
            }
        },
    }
}

/// Pure decision: the command a subtitle event turns into, or `None` for
/// "send nothing at all".
///
/// Mirrors exactly what the overlay renders as its current line:
///   * `Replace` — a provider's *cumulative* revision of the open sentence;
///   * `Final`   — the settled sentence;
///   * `Cleared` — blanks the source, exactly like the overlay's clear.
/// `Partial` is deliberately **not** mirrored: it arrives token by token, and
/// the text source is a sentence-level display, so mirroring it would mean a
/// frame per token for no visible benefit. Empty text never mirrors (it would
/// only blank a source that is already blank).
pub fn mirror_command(enabled: bool, ev: &SubtitleEvent) -> Option<ObsCommand> {
    mirror_text_for(enabled, ev).map(|text| ObsCommand::UpdateTextSource {
        name: MIRROR_INPUT_NAME.to_string(),
        text,
    })
}

/// The text a subtitle event mirrors (see [`mirror_command`]) — the pure part
/// the tests pin without touching a socket.
pub fn mirror_text_for(enabled: bool, ev: &SubtitleEvent) -> Option<String> {
    if !enabled {
        return None;
    }
    match ev {
        SubtitleEvent::Final(text) | SubtitleEvent::Replace(text) if !text.is_empty() => {
            Some(text.clone())
        }
        SubtitleEvent::Cleared => Some(String::new()),
        // Everything else mirrors nothing. In particular the hub's line-record
        // variant (`FinalLine { .. }`) is ignored on purpose: the wire feed
        // already carries the settled text as `Final`, so matching both would
        // mirror every line twice.
        _ => None,
    }
}

/// Deliver a command through a shared handle without ever blocking.
///
/// * `Err(NotConnected)` when the handle is empty (OBS not connected);
/// * `Err(QueueFull)` when the connection's queue is full — dropped on purpose,
///   because a subtitle must never wait for OBS;
/// * a channel that is already closed withdraws the dead handle and reports
///   `NotConnected`, so a stale sender cannot look like a working one.
pub fn try_send_command(
    slot: &Mutex<Option<CmdTx>>,
    cmd: ObsCommand,
) -> std::result::Result<(), SendCommandError> {
    let Some(tx) = slot.lock().clone() else {
        return Err(SendCommandError::NotConnected);
    };
    match tx.try_send(cmd) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(SendCommandError::QueueFull),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            let mut guard = slot.lock();
            if guard
                .as_ref()
                .is_some_and(|current| current.same_channel(&tx))
            {
                *guard = None;
            }
            Err(SendCommandError::NotConnected)
        }
    }
}

async fn try_connect(
    cfg: &ObsConfig,
    shared: &Arc<dyn ObsShared>,
    admin_url: Option<String>,
) -> Result<()> {
    let url = format!("ws://{}:{}", cfg.host, cfg.port);
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connect to OBS at {url}"))?;

    let hello = read_json(&mut ws).await?;
    if hello.get("op").and_then(|v| v.as_i64()) != Some(0) {
        return Err(anyhow!("unexpected OBS hello op"));
    }
    let d = hello.get("d").cloned().unwrap_or_default();
    let rpc_version = d.get("rpcVersion").and_then(|v| v.as_i64()).unwrap_or(1) as u32;
    let auth = d.get("authentication").cloned();
    let auth_required = auth.is_some();

    let identify_d = if let Some(auth) = auth {
        let challenge = auth
            .get("challenge")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("OBS auth challenge missing"))?
            .to_string();
        let salt = auth
            .get("salt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("OBS auth salt missing"))?
            .to_string();
        let secret = compute_auth(&cfg.password, &salt, &challenge);
        serde_json::json!({
            "rpcVersion": rpc_version,
            "authentication": secret,
            "eventSubscriptions": 33,
        })
    } else {
        serde_json::json!({
            "rpcVersion": rpc_version,
            "eventSubscriptions": 33,
        })
    };
    ws.send(Message::Text(
        serde_json::json!({"op": 1, "d": identify_d})
            .to_string()
            .into(),
    ))
    .await?;

    let identified = read_json(&mut ws).await.map_err(|e| {
        if auth_required {
            anyhow!(
                "OBS 拒绝了认证（通常是 OBS WebSocket 密码不匹配：请在 工具 → WebSocket 服务器设置 中核对，或取消勾选“启用身份验证”）: {e}"
            )
        } else {
            anyhow!("OBS closed during handshake: {e}")
        }
    })?;
    if identified.get("op").and_then(|v| v.as_i64()) != Some(2) {
        return Err(anyhow!("OBS did not confirm identify"));
    }
    info!(rpc = rpc_version, "OBS WebSocket identified");

    shared.set_connected(Some(format!("rpc {rpc_version}")));

    let (mut write_half, mut read_half) = ws.split();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(32);
    // Publish before anything else can produce a line to mirror: from here on
    // every caller either gets this connection's sender or `None`.
    shared.set_cmd_tx(Some(cmd_tx));

    if cfg.register_dock {
        if let Some(url) = admin_url {
            if let Err(e) = ws_create_admin_browser_source(&mut write_half, &url).await {
                warn!(error = %e, "failed to auto-create admin browser source");
            } else {
                info!(
                    input = DOCK_INPUT_NAME,
                    "admin panel registered as in-OBS browser source"
                );
            }
        }
    }

    let read_shared = shared.clone();
    let mut read_handle = tokio::spawn(async move {
        while let Some(msg) = read_half.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if v.get("op").and_then(|v| v.as_i64()) == Some(5) {
                            if let Some(es) = v
                                .get("d")
                                .and_then(|d| d.get("eventType"))
                                .and_then(|s| s.as_str())
                            {
                                tracing::debug!(event = %es, "OBS event");
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "OBS ws read error");
                    break;
                }
                _ => {}
            }
        }
        read_shared.set_disconnected(Some(
            "OBS WebSocket 连接已断开（引擎会自动重连）".to_string(),
        ));
    });

    let write_shared = shared.clone();
    let mut write_handle = tokio::spawn(async move {
        // Per connection: the text source is created once, then only updated.
        let mut text_input_created = false;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                ObsCommand::UpdateTextSource { name, text } => {
                    if let Err(e) =
                        ws_set_text(&mut write_half, &name, &text, &mut text_input_created).await
                    {
                        write_shared.set_last_error(Some(e.to_string()));
                        // The socket itself is broken; tear the connection down
                        // instead of queueing writes into a dead socket.
                        break;
                    }
                }
                ObsCommand::BroadcastEvent { event, data } => {
                    let req = serde_json::json!({
                        "op": 6,
                        "d": {
                            "requestType": "BroadcastCustomEvent",
                            "requestId": uuid::Uuid::new_v4().to_string(),
                            "requestData": {
                                "eventData": { "event": event, "data": data }
                            }
                        }
                    });
                    if write_half
                        .send(Message::Text(req.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ObsCommand::OpenAdminInObs => {
                    if let Err(e) = ws_open_admin_interact(&mut write_half).await {
                        write_shared.set_last_error(Some(e.to_string()));
                        break;
                    }
                }
                ObsCommand::Shutdown => break,
            }
        }
    });

    // Exactly one half has to end for this connection to be over. The old code
    // used `join!`, which waited for BOTH: when the reader ended (OBS closed),
    // the writer kept waiting on `cmd_rx` forever, `try_connect` never
    // returned, the loop never re-dialed and the OBS dot stayed red until the
    // process was restarted. Cancel the survivor instead.
    let read_ended = tokio::select! {
        _ = &mut read_handle => true,
        _ = &mut write_handle => false,
    };
    let survivor = if read_ended {
        write_handle.abort();
        &mut write_handle
    } else {
        read_handle.abort();
        &mut read_handle
    };
    // Aborting is immediate, but bound the wait anyway: the reconnect loop must
    // resume within DISCONNECT_GRACE even if a half is wedged.
    let _ = tokio::time::timeout(DISCONNECT_GRACE, survivor).await;

    // No live connection any more: withdraw the handle so no caller keeps
    // sending into a dead channel, and never leave the panel "connected".
    shared.set_cmd_tx(None);
    shared.set_disconnected(None);
    Ok(())
}

/// Push one line into the OBS text source and make sure it exists.
///
/// The input is created **once per connection**, lazily on the first update:
/// at connect time we do not know whether mirroring is on, and dropping an
/// unused text source into the user's scene would be intrusive. The old code
/// re-sent `CreateInput` on *every* update, so OBS answered with "an input with
/// this name already exists" for the rest of the session.
///
/// `name` is both the input that is updated and the name of the lazily created
/// fallback, so a source the user already created (the documented
/// [`MIRROR_INPUT_NAME`]) is updated in place and never duplicated.
/// `created` is the per-connection flag owned by the command loop.
///
/// Input kind / font stay platform specific: the Windows text source is GDI+
/// (`OBS_TEXT_KIND`) and that kind cannot be created on Linux/macOS at all.
async fn ws_set_text<W>(
    write_half: &mut W,
    name: &str,
    text: &str,
    created: &mut bool,
) -> Result<()>
where
    W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    if !*created {
        let create = serde_json::json!({
            "op": 6,
            "d": {
                "requestType": "CreateInput",
                "requestId": uuid::Uuid::new_v4().to_string(),
                "requestData": {
                    "sceneName": "Current Scene",
                    "inputName": name,
                    "inputKind": OBS_TEXT_KIND,
                    "inputSettings": {
                        "text": text,
                        "font": { "face": OBS_TEXT_FONT, "size": 48 },
                        "color": 0xFFFFFFFFu32,
                        "outline": true,
                        "outline_color": 0xFF000000u32,
                        "outline_size": 2
                    }
                }
            }
        });
        write_half
            .send(Message::Text(create.to_string().into()))
            .await?;
        // Created (or it already existed — either way, never again this
        // connection).
        *created = true;
    }

    let update = serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetInputSettings",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "requestData": { "inputName": name, "settings": { "text": text } }
        }
    });
    write_half
        .send(Message::Text(update.to_string().into()))
        .await?;
    Ok(())
}

async fn ws_create_admin_browser_source<W>(write_half: &mut W, url: &str) -> Result<()>
where
    W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let create = serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "CreateInput",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "requestData": {
                "sceneName": "Current Scene",
                "inputName": DOCK_INPUT_NAME,
                "inputKind": "browser_source",
                "inputSettings": {
                    "url": url,
                    "width": 480,
                    "height": 640,
                    "is_local_file": false,
                    "restart_when_active": true,
                    "css": "body{background:transparent;}"
                },
                "sceneItemEnabled": false
            }
        }
    });
    let _ = write_half
        .send(Message::Text(create.to_string().into()))
        .await;

    let update = serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetInputSettings",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "requestData": {
                "inputName": DOCK_INPUT_NAME,
                "settings": {
                    "url": url,
                    "width": 480,
                    "height": 640,
                    "is_local_file": false,
                    "restart_when_active": true,
                    "css": "body{background:transparent;}"
                }
            }
        }
    });
    write_half
        .send(Message::Text(update.to_string().into()))
        .await?;
    Ok(())
}

async fn ws_open_admin_interact<W>(write_half: &mut W) -> Result<()>
where
    W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let req = serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "OpenInputInteract",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "requestData": { "inputName": DOCK_INPUT_NAME }
        }
    });
    write_half
        .send(Message::Text(req.to_string().into()))
        .await?;
    Ok(())
}

async fn read_json<S>(read_half: &mut S) -> Result<serde_json::Value>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = read_half.next().await {
        match msg? {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    return Ok(v);
                }
            }
            Message::Close(_) => return Err(anyhow!("OBS closed during handshake")),
            _ => {}
        }
    }
    Err(anyhow!("OBS connection closed"))
}

fn compute_auth(password: &str, salt: &str, challenge: &str) -> String {
    let mut h1 = Sha256::new();
    h1.update(password.as_bytes());
    h1.update(salt.as_bytes());
    let step1 = h1.finalize();
    let step2_b64 = base64::engine::general_purpose::STANDARD.encode(step1);
    let mut h3 = Sha256::new();
    h3.update(step2_b64.as_bytes());
    h3.update(challenge.as_bytes());
    let step3 = h3.finalize();
    base64::engine::general_purpose::STANDARD.encode(step3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc as tmpsc;

    type Frames = tmpsc::UnboundedSender<serde_json::Value>;

    /// Wall-clock budget for one bounded wait. Generous on purpose: this machine
    /// may be busy compiling the rest of the crate, and the regression this
    /// guards against — a `join!` that never returns — burns the whole budget.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// How long the "nothing else arrives" assertions listen for.
    const QUIET: Duration = Duration::from_millis(250);

    /// Stand-in for `AppState` + the `ObsClient` cells: exactly the shared state
    /// a connection publishes into, without recording/pipeline/tokens.
    #[derive(Default)]
    struct TestShared {
        cmd_tx: Mutex<Option<CmdTx>>,
        status: Mutex<ObsStatus>,
    }

    impl TestShared {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn is_connected(&self) -> bool {
            self.status.lock().connected
        }
        fn live_sender(&self) -> Option<CmdTx> {
            self.cmd_tx.lock().clone()
        }
    }

    impl ObsShared for TestShared {
        fn set_cmd_tx(&self, tx: Option<CmdTx>) {
            *self.cmd_tx.lock() = tx;
        }
        fn set_connected(&self, version: Option<String>) {
            let mut s = self.status.lock();
            s.connected = true;
            s.version = version;
            s.last_error = None;
        }
        fn set_last_error(&self, err: Option<String>) {
            self.status.lock().last_error = err;
        }
        fn set_disconnected(&self, err: Option<String>) {
            let mut s = self.status.lock();
            s.connected = false;
            if err.is_some() {
                s.last_error = err;
            }
        }
    }

    /// How a fake OBS connection should end.
    enum Behaviour {
        /// Close the socket immediately after the handshake — "OBS was closed".
        CloseAfterHandshake,
        /// Stay connected until the test fires this signal, then close. Lets a
        /// test observe the live connection before OBS goes away.
        CloseOnSignal(tokio::sync::oneshot::Receiver<()>),
        /// Stay connected, reporting every request frame, until the client
        /// leaves.
        Stay,
    }

    /// A fake OBS WebSocket v5 server for one connection.
    ///
    /// Does the real handshake (hello `op:0` -> read the client's identify
    /// `op:1` -> identified `op:2`) and then follows `behaviour`, forwarding
    /// every request frame (`op:6`) it receives to the test.
    async fn fake_obs_conn(listener: &TcpListener, behaviour: Behaviour, frames: Frames) {
        let (tcp, _) = listener.accept().await.expect("fake OBS accept");
        let mut ws = tokio_tungstenite::accept_async(tcp)
            .await
            .expect("fake OBS ws handshake");
        ws.send(Message::Text(
            serde_json::json!({"op": 0, "d": {"rpcVersion": 1}})
                .to_string()
                .into(),
        ))
        .await
        .expect("fake OBS hello");

        let identify = read_json(&mut ws).await.expect("client identify");
        assert_eq!(
            identify.get("op").and_then(|v| v.as_i64()),
            Some(1),
            "client must identify first: {identify}"
        );
        ws.send(Message::Text(
            serde_json::json!({"op": 2, "d": {"negotiatedRpcVersion": 1}})
                .to_string()
                .into(),
        ))
        .await
        .expect("fake OBS identified");

        match behaviour {
            Behaviour::CloseAfterHandshake => {
                let _ = ws.close(None).await;
                return;
            }
            Behaviour::CloseOnSignal(signal) => {
                tokio::select! {
                    _ = ws.next() => {}
                    _ = signal => {}
                }
                let _ = ws.close(None).await;
                return;
            }
            Behaviour::Stay => {}
        }

        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        let _ = frames.send(v);
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    }

    fn obs_cfg(addr: SocketAddr) -> ObsConfig {
        let mut cfg = Config::default().obs;
        cfg.host = addr.ip().to_string();
        cfg.port = addr.port();
        // No dock registration: keeps the frame stream in these tests to just
        // what the mirror produces.
        cfg.register_dock = false;
        cfg
    }

    async fn bind_fake_obs() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        (listener, addr)
    }

    async fn wait_until<F: Fn() -> bool>(what: &str, cond: F) {
        let deadline = tokio::time::Instant::now() + TEST_TIMEOUT * 2;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    async fn next_frame(
        frames: &mut tmpsc::UnboundedReceiver<serde_json::Value>,
    ) -> serde_json::Value {
        tokio::time::timeout(TEST_TIMEOUT, frames.recv())
            .await
            .expect("timed out waiting for a frame from the engine")
            .expect("frame channel closed")
    }

    /// End a session the way the engine does — `Shutdown` through the shared
    /// handle — and wait (bounded) for the connection task and the fake server
    /// to notice. Aborting the connection task instead would only detach the
    /// read/write tasks it spawned, which keep the socket open.
    async fn shutdown_session(
        shared: &Arc<TestShared>,
        conn: tokio::task::JoinHandle<Result<()>>,
        server: tokio::task::JoinHandle<()>,
    ) {
        let _ = try_send_command(&shared.cmd_tx, ObsCommand::Shutdown);
        let ended = tokio::time::timeout(TEST_TIMEOUT, conn)
            .await
            .expect("try_connect must return after Shutdown")
            .expect("connection task join");
        assert!(ended.is_ok(), "{ended:?}");
        let _ = tokio::time::timeout(TEST_TIMEOUT, server)
            .await
            .expect("the fake OBS must see the connection end");
    }

    /// `op:6` request body (`d`), with the op asserted.
    fn request_body(v: &serde_json::Value) -> serde_json::Value {
        assert_eq!(
            v.get("op").and_then(|o| o.as_i64()),
            Some(6),
            "expected an op:6 request frame, got {v}"
        );
        v.get("d").cloned().unwrap_or_default()
    }

    /// The pure half of the mirror decision: the setting is opt-in (the config
    /// default is `false`) and every event type sends nothing while it is off.
    #[test]
    fn mirror_is_off_by_default_and_sends_nothing() {
        assert!(
            !Config::default().overlay.mirror_to_text_source,
            "mirroring must stay opt-in"
        );
        let events = [
            SubtitleEvent::Partial("你好".to_string()),
            SubtitleEvent::Replace("你好，世界".to_string()),
            SubtitleEvent::Final("你好，世界。".to_string()),
            SubtitleEvent::Cleared,
        ];
        for ev in &events {
            assert!(
                mirror_command(false, ev).is_none(),
                "disabled mirroring must not produce a command for {ev:?}"
            );
            assert!(mirror_text_for(false, ev).is_none(), "{ev:?}");
        }
    }

    /// …and nothing at all reaches the wire while it is off, even on a live
    /// connection.
    #[tokio::test]
    async fn mirror_off_puts_no_frame_on_the_wire() {
        let (listener, addr) = bind_fake_obs().await;
        let (frames_tx, mut frames_rx) = tmpsc::unbounded_channel();
        let server =
            tokio::spawn(async move { fake_obs_conn(&listener, Behaviour::Stay, frames_tx).await });

        let cfg = obs_cfg(addr);
        let shared = TestShared::new();
        let conn_shared: Arc<dyn ObsShared> = shared.clone();
        let conn = tokio::spawn(async move { try_connect(&cfg, &conn_shared, None).await });
        wait_until("the client to connect", || shared.live_sender().is_some()).await;

        for ev in [
            SubtitleEvent::Partial("你".to_string()),
            SubtitleEvent::Replace("你好".to_string()),
            SubtitleEvent::Final("你好。".to_string()),
            SubtitleEvent::Cleared,
        ] {
            assert!(!mirror_one(false, &ev, &shared.cmd_tx), "{ev:?}");
        }

        assert!(
            tokio::time::timeout(QUIET, frames_rx.recv()).await.is_err(),
            "a disabled mirror must not send anything to OBS"
        );

        shutdown_session(&shared, conn, server).await;
    }

    /// The wired-up mirror: a Final reaches OBS as a `SetInputSettings` for
    /// [`MIRROR_INPUT_NAME`] carrying the same text, and the fallback
    /// `CreateInput` happens at most once per connection.
    #[tokio::test]
    async fn mirror_sends_set_input_settings_for_a_final_line() {
        let (listener, addr) = bind_fake_obs().await;
        let (frames_tx, mut frames_rx) = tmpsc::unbounded_channel();
        let server =
            tokio::spawn(async move { fake_obs_conn(&listener, Behaviour::Stay, frames_tx).await });

        let cfg = obs_cfg(addr);
        let shared = TestShared::new();
        let conn_shared: Arc<dyn ObsShared> = shared.clone();
        let conn = tokio::spawn(async move { try_connect(&cfg, &conn_shared, None).await });
        wait_until("the client to connect", || shared.live_sender().is_some()).await;

        assert!(
            mirror_one(
                true,
                &SubtitleEvent::Final("你好，世界。".to_string()),
                &shared.cmd_tx
            ),
            "an enabled mirror must hand the line to the connection"
        );
        assert!(mirror_one(
            true,
            &SubtitleEvent::Final("第二句。".to_string()),
            &shared.cmd_tx
        ));

        // The lazy create goes first, then one update per line.
        let created = request_body(&next_frame(&mut frames_rx).await);
        assert_eq!(created["requestType"], "CreateInput");
        assert_eq!(created["requestData"]["inputName"], MIRROR_INPUT_NAME);
        assert_eq!(created["requestData"]["inputKind"], OBS_TEXT_KIND);

        let first = request_body(&next_frame(&mut frames_rx).await);
        assert_eq!(first["requestType"], "SetInputSettings");
        assert_eq!(first["requestData"]["inputName"], MIRROR_INPUT_NAME);
        assert_eq!(first["requestData"]["settings"]["text"], "你好，世界。");

        let second = request_body(&next_frame(&mut frames_rx).await);
        assert_eq!(second["requestType"], "SetInputSettings");
        assert_eq!(second["requestData"]["settings"]["text"], "第二句。");

        // No second CreateInput for the rest of the connection.
        assert!(
            tokio::time::timeout(QUIET, frames_rx.recv()).await.is_err(),
            "the fallback CreateInput must be sent at most once per connection"
        );

        shutdown_session(&shared, conn, server).await;
    }

    /// The regression test for defect #1: with `join!` this never returns.
    #[tokio::test]
    async fn closing_the_reader_ends_try_connect() {
        let (listener, addr) = bind_fake_obs().await;
        let (frames_tx, _frames_rx) = tmpsc::unbounded_channel();
        // Handshake, then close the socket: "OBS was closed".
        let server = tokio::spawn(async move {
            fake_obs_conn(&listener, Behaviour::CloseAfterHandshake, frames_tx).await
        });

        let cfg = obs_cfg(addr);
        let shared = TestShared::new();
        let conn_shared: Arc<dyn ObsShared> = shared.clone();

        // Bounded: a regression has to show up as a failure, never as a hung
        // test run.
        let outcome =
            tokio::time::timeout(TEST_TIMEOUT, try_connect(&cfg, &conn_shared, None)).await;

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("try_connect failed instead of returning after OBS closed: {e:#}"),
            Err(_) => panic!(
                "try_connect never returned after OBS closed the connection \
                 (the read half ended, the write half kept waiting on cmd_rx)"
            ),
        }

        assert!(
            !shared.is_connected(),
            "status must report disconnected once the connection is over"
        );
        assert!(
            shared.live_sender().is_none(),
            "the dead command handle must not stay published"
        );

        let _ = server.await;
    }

    /// Defect #2: the shared handle must be republished on every connect and
    /// withdrawn on every disconnect, so a command sent through it reaches the
    /// *new* connection — and so the pre-reconnect handle is reported dead
    /// instead of quietly swallowing commands.
    #[tokio::test]
    async fn the_shared_command_sender_survives_a_reconnect() {
        let (listener, addr) = bind_fake_obs().await;
        let (frames_tx, mut frames_rx) = tmpsc::unbounded_channel();
        let (close_first, close_first_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            // First connection: stays up until the test lets OBS "die".
            fake_obs_conn(
                &listener,
                Behaviour::CloseOnSignal(close_first_rx),
                frames_tx.clone(),
            )
            .await;
            // Second connection: stays up and reports what it receives.
            fake_obs_conn(&listener, Behaviour::Stay, frames_tx).await;
        });

        let cfg = obs_cfg(addr);
        let shared = TestShared::new();

        // ---- connection 1: publish, then lose OBS ---------------------------
        let first_shared: Arc<dyn ObsShared> = shared.clone();
        let first_cfg = cfg.clone();
        let first = tokio::spawn(async move { try_connect(&first_cfg, &first_shared, None).await });
        wait_until("the first connection", || shared.live_sender().is_some()).await;
        let stale = shared
            .live_sender()
            .expect("a connected client publishes a sender");

        // OBS goes away.
        let _ = close_first.send(());

        let first_result = tokio::time::timeout(TEST_TIMEOUT, first)
            .await
            .expect("try_connect must return after OBS closed")
            .expect("join");
        assert!(first_result.is_ok(), "{first_result:?}");
        assert!(
            shared.live_sender().is_none(),
            "a disconnect must withdraw the shared handle"
        );
        wait_until("the stale channel to close", || stale.is_closed()).await;

        // ---- connection 2: the handle must work again -----------------------
        let second_shared: Arc<dyn ObsShared> = shared.clone();
        let second_cfg = cfg.clone();
        let second =
            tokio::spawn(async move { try_connect(&second_cfg, &second_shared, None).await });
        wait_until("the reconnected session", || shared.live_sender().is_some()).await;
        let live = shared
            .live_sender()
            .expect("a reconnect must republish a live sender");
        assert!(
            !live.same_channel(&stale),
            "the shared handle must be the new connection's channel, not the dead one"
        );

        // Exactly what the mirror / any other caller does: `try_send` through
        // the shared handle, never blocking.
        try_send_command(
            &shared.cmd_tx,
            ObsCommand::UpdateTextSource {
                name: MIRROR_INPUT_NAME.to_string(),
                text: "重连之后的字幕".to_string(),
            },
        )
        .expect("a command through the shared handle must be accepted");

        let mut reached_new_connection = false;
        for _ in 0..2 {
            let body = request_body(&next_frame(&mut frames_rx).await);
            if body["requestType"] == "SetInputSettings"
                && body["requestData"]["inputName"] == MIRROR_INPUT_NAME
                && body["requestData"]["settings"]["text"] == "重连之后的字幕"
            {
                reached_new_connection = true;
                break;
            }
        }
        assert!(
            reached_new_connection,
            "the command sent through the shared handle never reached the reconnected session"
        );

        // And a handle captured before the drop-out is reported dead, not
        // silently "sent".
        assert!(stale.try_send(ObsCommand::Shutdown).is_err());

        shutdown_session(&shared, second, server).await;
    }
}
