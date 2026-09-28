// Windows: hide the console window when launched from OBS plugin (CreateProcessA
// already uses CREATE_NO_WINDOW, but Rust still defaults to the console subsystem
// which can briefly pop a window on startup or whenever something writes to
// stderr). On other targets this attribute is a no-op.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

pub mod audio;
pub mod auth;
pub mod config;
pub mod embedded;
pub mod hotwords;
pub mod ingest;
pub mod lang;
pub mod llm;
pub mod obs;
pub mod pipeline;
pub mod recording;
pub mod server;
pub mod subtitle;
pub mod vad;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use tracing::{info, warn};

use crate::config::Config;

static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn config_path() -> PathBuf {
    CONFIG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

#[derive(Parser, Debug)]
#[command(
    name = "stream-live-translate",
    version,
    about = "Real-time AI subtitle overlay for OBS Studio"
)]
struct Cli {
    #[arg(long, short = 'c', global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    host: Option<String>,
    #[arg(long, short = 'p', global = true)]
    port: Option<u16>,
    #[arg(long)]
    open: bool,
    #[arg(long)]
    headless: bool,
    /// Override `audio.mode` (e.g. the OBS plugin passes `obs_filter`).
    /// The value is persisted into config.toml.
    #[arg(long, global = true)]
    audio_mode: Option<String>,
    /// Temporary local PCM ingest port. Unlike --audio-mode this override is
    /// deliberately not persisted, so a replay run cannot alter an OBS setup.
    #[arg(long, global = true)]
    ingest_port: Option<u16>,
    /// Admin token for the local HTTP API (P0-01). Env: `SLT_ADMIN_TOKEN`.
    /// Supply both tokens together to allow a non-loopback `--host`.
    #[arg(long, global = true, env = "SLT_ADMIN_TOKEN")]
    admin_token: Option<String>,
    /// Read-only token handed to the overlay / OBS Browser Source.
    /// Env: `SLT_OVERLAY_TOKEN`.
    #[arg(long, global = true, env = "SLT_OVERLAY_TOKEN")]
    overlay_token: Option<String>,
    /// One-time nonce the OBS plugin passes so the ingest port can verify it is
    /// talking to this engine (P0-05). Not persisted.
    #[arg(long, global = true, env = "SLT_INGEST_NONCE")]
    ingest_nonce: Option<String>,
}

pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    /// Notifies connected overlays that the overlay style config changed so
    /// they can restyle themselves live — the user no longer has to copy a
    /// fresh browser-source URL after tweaking the background settings.
    /// The payload is empty on purpose: receivers re-read `state.config`,
    /// which guarantees they always get the newest value.
    pub config_tx: tokio::sync::broadcast::Sender<()>,
    pub subtitle: Arc<subtitle::SubtitleHub>,
    pub pipeline: Arc<pipeline::PipelineHandle>,
    pub status: Arc<RwLock<AppStatus>>,
    pub recording: recording::RecordingStore,
    pub obs_cmd_tx: parking_lot::Mutex<Option<tokio::sync::mpsc::Sender<crate::obs::ObsCommand>>>,
    /// Audio mode forced via `--audio-mode` (the OBS plugin passes
    /// `obs_filter`). While set, admin-panel config patches can never
    /// change `audio.mode`, so saving other settings can't break the
    /// audio feed.
    pub forced_audio_mode: Option<String>,
    /// Consecutive provider-session failures since the last time a session
    /// actually reached the provider. Drives the exponential reconnect backoff,
    /// which is capped so a permanently dead endpoint settles at a long interval
    /// instead of retrying forever at a fixed short one. Reset whenever
    /// `llm_connected` is observed true.
    pub provider_failures: std::sync::atomic::AtomicU32,
    /// 热词下发状态（R10）：管理页据此显示"已下发 N 个热词 / 上次生效时间 /
    /// 未变化未重发"。只由 provider 写入，不含任何密钥。
    pub hotword_status: Arc<RwLock<crate::hotwords::HotwordStatus>>,
    /// 运行期热词源（R10）：管理页保存 → 这里 → provider 的 continue-task。
    pub hotwords: crate::hotwords::HotwordFeed,
    /// 上一次保存时 `llm.speech_noise_threshold` 是否被钳制过。
    ///
    /// 必须单独记：一旦钳过，磁盘与内存里存的就是边界值本身，"值是否越界"
    /// 再也看不出来。管理页要如实告诉用户"你填的值超出 −1~1，实际按边界值
    /// 下发"，只能靠这个运行期标记。用户下次保存一个合法值时会清掉。
    pub speech_noise_threshold_clamped: std::sync::atomic::AtomicBool,
    /// Access tokens for the HTTP/WebSocket server (P0-01). The overlay token is
    /// read-only and separate from the admin token, so the URL pasted into an
    /// OBS Browser Source cannot change settings or stop recognition.
    pub tokens: crate::auth::TokenPair,
    /// `host:port` the server actually serves on, used to validate the `Origin`
    /// header against this server's own origin.
    pub bind_addr: (String, u16),
    /// True when the operator deliberately configured authentication (env var or
    /// a pre-existing `tokens.toml`). A token this process generated for itself
    /// is not evidence that binding a public interface was intended, so
    /// `auth::bind_policy` uses this to decide whether to refuse startup.
    pub auth_configured: bool,
    /// 外送链路信任域（P0-02）：内存中 `llm.api_key` 当前被允许发往的端点主机。
    /// 端点换成另一个信任域时，旧 Key 必须失效并要求用户重新确认/输入，而不是
    /// 被静默发往新主机。
    pub llm_key_domain: parking_lot::Mutex<Option<String>>,
    /// OBS 插件通过 `--ingest-nonce` 一次性传入的握手身份（P0-05）。
    pub ingest_nonce: Option<Arc<str>>,
    /// Serialises the whole read-modify-write cycle of a config save (P2-02):
    /// two concurrent `POST /api/config` requests must not interleave into a
    /// lost update.
    pub config_write_lock: parking_lot::Mutex<()>,
    /// Absolute path of the config file this process loaded.
    ///
    /// Held here rather than only in the `CONFIG_PATH` `OnceLock` so the request
    /// handlers can validate a patch against the directory the config really
    /// lives in. The `OnceLock` cannot be set by a test, and validating against
    /// its default (`"config.toml"`, whose parent is `.`) made every absolute
    /// `recording_dir` look "inside the config directory" — the check silently
    /// passed for exactly the case it exists to refuse.
    pub config_path: std::path::PathBuf,
}

#[derive(Default, Clone, Debug)]
pub struct AppStatus {
    pub audio_active: bool,
    /// RMS of the most recent captured frame (0.0–1.0). Measured before VAD
    /// so the panel can show quiet speech even when a filter suppresses it.
    pub input_level: f32,
    /// Rolling capture counters reset only by the explicit microphone test.
    pub input_frames: u64,
    pub input_rms_sum: f64,
    pub input_peak: f32,
    /// Wall-clock time of the newest captured frame. Lets the watcher tell a
    /// live input from one that silently stopped producing frames (OBS scene
    /// switch, unplugged receiver) without waiting for the channel to close.
    pub last_input_at: Option<chrono::DateTime<chrono::Utc>>,
    pub llm_connected: bool,
    pub obs_connected: bool,
    pub last_error: Option<String>,
    /// Latest OBS WebSocket connection failure (surfaced in the admin
    /// panel so users know why the OBS dot is red).
    pub obs_error: Option<String>,
    pub last_subtitle_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate current executable")?;
    Ok(exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?
        .to_path_buf())
}

pub fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = cli_path {
        return Ok(p);
    }
    let dir = exe_dir()?;
    let portable = dir.join("config.toml");
    let probe = dir.join(".stream-live-translate-write-probe");
    let writable = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map(|_| {
            let _ = std::fs::remove_file(&probe);
            true
        })
        .unwrap_or(false);
    if writable {
        return Ok(portable);
    }
    if let Some(mut user_dir) = dirs::config_dir() {
        user_dir.push("stream-live-translate");
        return Ok(user_dir.join("config.toml"));
    }
    Ok(portable)
}

pub fn resolve_static_dir() -> Result<PathBuf> {
    let dir = exe_dir()?;
    Ok(dir.join("dist"))
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let cfg_path = resolve_config_path(cli.config.clone()).context("resolve config path")?;
    let _ = CONFIG_PATH.set(cfg_path.clone());
    info!(
        path = %cfg_path.display(),
        exe_dir = %exe_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "loading config (portable mode: config lives next to the executable)"
    );
    let mut cfg = Config::load_or_create(&cfg_path)
        .with_context(|| format!("load config {}", cfg_path.display()))?;

    // P0-04 upgrade path: a config on disk was written before these boundaries
    // existed, so it can hold a value a *patch* would now be rejected for. The
    // one that really shipped is `audio.sample_rate = 0`, which used to mean
    // "device default" and now has no meaning — every producer emits a fixed
    // internal rate, so the value the VAD is told would disagree with the rate
    // the frames were made at. Those files would otherwise fail validation and
    // silently stop recognising audio, so the legacy sentinel is rewritten once,
    // loudly. Everything else that cannot be normalised safely is reported and
    // left untouched.
    if cfg.audio.sample_rate == 0 {
        warn!(
            "config.toml 里 audio.sample_rate = 0（旧版“设备默认”写法）已不再支持，\
             已按内部固定采样率 {} Hz 纠正；请到管理页保存一次以写回磁盘",
            crate::pipeline::INTERNAL_SAMPLE_RATE
        );
        cfg.audio.sample_rate = crate::pipeline::INTERNAL_SAMPLE_RATE;
    }
    match cfg.validate(&cfg_path) {
        Ok(()) => {}
        Err(errors) => {
            warn!(errors = %errors, "config.toml 存在超出允许范围的值；管线会拒绝启动，请在管理页修正");
        }
    }

    if let Ok(dir) = resolve_static_dir() {
        cfg.server.static_dir = dir;
    }

    if let Some(host) = &cli.host {
        cfg.server.host = host.clone();
    }
    if let Some(port) = cli.port {
        cfg.server.port = port;
    }
    if let Some(mode) = &cli.audio_mode {
        cfg.audio.mode = mode.clone();
        if let Err(e) = cfg.save(&cfg_path) {
            warn!(error = %e, "failed to persist audio mode override");
        } else {
            info!(mode = %mode, "audio mode overridden by CLI");
        }
    }
    if let Some(port) = cli.ingest_port {
        cfg.audio.ingest_port = port;
        info!(port, "audio ingest port overridden for this process only");
    }

    // ---------------------------------------------------------------------
    // Access control (P0-01). Decided BEFORE anything is spawned: a bind that
    // would expose the admin API without authentication must stop the process,
    // not log a warning and carry on.
    // ---------------------------------------------------------------------
    let tokens_file = auth::tokens_path(&cfg_path);
    let had_token_file = tokens_file.exists();
    let tokens = match (&cli.admin_token, &cli.overlay_token) {
        // Both supplied from the environment / command line.
        (Some(admin), Some(overlay)) => {
            let pair = auth::TokenPair {
                admin: admin.clone(),
                overlay: overlay.clone(),
            };
            if !pair.is_valid() {
                anyhow::bail!(
                    "SLT_ADMIN_TOKEN 和 SLT_OVERLAY_TOKEN 必须是不同的 64 位十六进制令牌"
                );
            }
            pair
        }
        (None, None) => auth::load_or_create(&cfg_path)
            .with_context(|| format!("create access tokens at {}", tokens_file.display()))?,
        _ => anyhow::bail!("必须同时提供 SLT_ADMIN_TOKEN 和 SLT_OVERLAY_TOKEN"),
    };
    let auth_configured = cli.admin_token.is_some() || had_token_file;
    if let auth::BindPolicy::Refuse(message) = auth::bind_policy(&cfg.server.host, auth_configured)
    {
        // Hard failure: exiting non-zero is the only honest outcome here.
        anyhow::bail!("{message}");
    }
    let bind_addr = (cfg.server.host.clone(), cfg.server.port);

    let subtitle = Arc::new(subtitle::SubtitleHub::default());
    let pipeline = Arc::new(pipeline::PipelineHandle::new());
    let status = Arc::new(RwLock::new(AppStatus::default()));

    let (config_tx, _config_rx) = tokio::sync::broadcast::channel::<()>(16);

    // 热词（R10）：启动时先用配置里的词表初始化 feed，Provider 一建会话就能带上。
    let hotwords = crate::hotwords::HotwordFeed::new();
    hotwords.set(crate::hotwords::plan(&cfg.llm.hotwords));

    // 外送链路（P0-02）：磁盘上的 Key 只对它当初配对的端点主机有效。
    let initial_key_domain = crate::llm::trust_domain(&cfg.llm);

    let state = Arc::new(AppState {
        config: Arc::new(RwLock::new(cfg.clone())),
        config_tx,
        subtitle: subtitle.clone(),
        pipeline: pipeline.clone(),
        status: status.clone(),
        recording: recording::RecordingStore::start(&cfg_path, &cfg.recording_dir),
        obs_cmd_tx: parking_lot::Mutex::new(None),
        forced_audio_mode: cli.audio_mode.clone(),
        provider_failures: std::sync::atomic::AtomicU32::new(0),
        hotword_status: Arc::new(RwLock::new(crate::hotwords::HotwordStatus::default())),
        hotwords,
        speech_noise_threshold_clamped: std::sync::atomic::AtomicBool::new(false),
        tokens: tokens.clone(),
        bind_addr: bind_addr.clone(),
        auth_configured,
        llm_key_domain: parking_lot::Mutex::new(initial_key_domain),
        ingest_nonce: cli.ingest_nonce.as_deref().map(Arc::from),
        config_write_lock: parking_lot::Mutex::new(()),
        config_path: cfg_path.clone(),
    });

    recording::spawn(state.recording.clone(), state.clone());

    // P1-06 / P2-07: apply the persistence switch and the retention policy that
    // the config (or the panel) asked for. Both are explicit: no recording is
    // deleted unless `retention_days` is non-zero, and turning persistence off
    // never removes what is already on disk.
    state.recording.set_enabled(cfg.auto_persist);
    if cfg.retention_days > 0 {
        let removed = state.recording.prune_older_than(cfg.retention_days);
        if removed > 0 {
            info!(
                removed,
                days = cfg.retention_days,
                "removed expired subtitle recordings"
            );
        }
    }

    pipeline::spawn(state.clone(), cfg_path.clone());

    let ingest_state = state.clone();
    tokio::spawn(async move {
        ingest::serve(ingest_state).await;
    });

    let obs_client = obs::spawn(state.clone());
    *state.obs_cmd_tx.lock() = obs_client.lock().sender();

    // The in-OBS dock hosts the admin panel, so it needs the *admin* token —
    // without it the browser source loads a panel that gets 401 on every call.
    // The token is injected into the page anyway, but the first navigation has
    // to carry it, and an OBS browser source is a plain URL.
    let admin_url = format!(
        "http://{}:{}/admin?obsDock=1&token={}",
        cfg.server.host, cfg.server.port, state.tokens.admin
    );
    obs_client.lock().set_admin_url(admin_url);

    let server_cfg = cfg.server.clone();
    let server_state = state.clone();
    let server_task = tokio::spawn(async move {
        if let Err(e) = server::serve(server_state, server_cfg).await {
            // Full cause chain: anyhow's plain Display would print only the
            // outermost context and hide the real reason.
            warn!(error = %format!("{e:#}"), "server exited");
        }
    });

    if cli.open {
        let url = format!(
            "http://{}:{}/admin?token={}",
            cfg.server.host, cfg.server.port, state.tokens.admin
        );
        if let Err(e) = open_in_browser(&url) {
            warn!(error = %e, "failed to open admin page");
        }
    }

    info!(
        host = %cfg.server.host,
        port = cfg.server.port,
        "stream-live-translate running"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
        _ = server_task => {
            warn!("server task ended unexpectedly");
        }
    }

    pipeline.shutdown().await;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stream_live_translate=info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}
