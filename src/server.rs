//! Local HTTP / WebSocket server.
//!
//! Endpoints:
//!   GET  /                      -> welcome
//!   GET  /admin                 -> SPA admin panel (HTML)
//!   GET  /overlay               -> browser source overlay (HTML)
//!   GET  /api/config            -> current config
//!   POST /api/config            -> save config (restarts pipeline if needed)
//!   GET  /api/devices           -> list audio devices
//!   GET  /api/status            -> { running, audio, llm, obs, last_error }
//!   GET  /api/auth/local-token  -> { token } for a panel opened without one,
//!                                  only ever answered on the loopback authority
//!   GET  /api/subtitles         -> current + history
//!   POST /api/subtitles/clear   -> clear current line
//!   POST /api/subtitles/history/clear -> clear current line + history
//!                                  (the panel's 銆屾竻绌哄巻鍙层€?button)
//!   GET  /api/recordings/export -> download this session's subtitles
//!   GET  /api/locale            -> { language } 鈥?host OS UI language
//!                                  ("zh" | "en"), used by the admin panel
//!                                  to pick its interface language
//!   WS   /ws/subtitles          -> live subtitle event stream

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;

/// How long an explicit config save / restart waits for a still-draining cloud
/// session before tearing it down. Short enough to feel immediate, long enough
/// for a `finish-task` drain to deliver its final sentence.
const SETTINGS_RESTART_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Application-level keepalive between the overlay and this server. The overlay
/// sends `ping` every 10 s and reconnects if nothing (not even this pong)
/// arrives for 30 s, which is what catches a half-open TCP connection that the
/// browser still reports as open.
const WS_PING_TEXT: &str = "ping";
const WS_PONG_TEXT: &str = "pong";
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{info, warn};

use crate::auth::{self, Access, Role};
use crate::config::{Config, ServerConfig};
use crate::embedded;
use crate::AppState;

/// Marker substituted with the caller's own token when an HTML page is served.
///
/// The token reaches the page as a `<script>` global rather than an injected
/// query parameter, so it never lands in a URL that the user might copy, log or
/// share. `admin/app.js` and `overlay/app.js` read it from `window`.
const TOKEN_PLACEHOLDER: &str = "__SLT_TOKEN__";

pub async fn serve(state: Arc<AppState>, cfg: ServerConfig) -> Result<()> {
    let static_dir: PathBuf = cfg.static_dir.clone();
    let app = build_router(state, static_dir);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("bad bind address: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    info!(addr = %bound, "server bound");
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_router(state: Arc<AppState>, static_dir: PathBuf) -> Router {
    // NOTE: these routes are nested under /api below, so the paths here
    // must NOT repeat the /api prefix.
    let api = Router::new()
        .route("/config", get(get_config).post(post_config))
        .route("/config/clear-key", post(clear_api_key))
        .route("/config/clear-obs-password", post(clear_obs_password))
        .route("/devices", get(get_devices))
        .route("/status", get(get_status))
        .route("/auth/local-token", get(get_local_token))
        .route("/audio-test", post(run_audio_test))
        .route("/connection-test", post(test_connection))
        .route("/subtitles", get(get_subtitles))
        .route("/subtitles/clear", post(clear_subtitles))
        .route("/subtitles/history/clear", post(clear_subtitle_history))
        .route("/restart", post(restart_pipeline))
        .route("/stop", post(stop_pipeline))
        .route("/locale", get(get_locale))
        .route(
            "/recordings",
            get(get_recording_info).delete(delete_recordings),
        )
        .route("/recordings/export", get(export_recording))
        .with_state(state.clone());

    // Disk directory for the optional bundled binaries (live-reload case).
    let bin_dir = static_dir.join("bin");

    // Panel/overlay static assets. The binary is self-contained: assets are
    // embedded at compile time. (We intentionally do NOT use ServeDir here:
    // pointing it at a nonexistent directory makes every request 404 without
    // ever reaching the fallback handler.)
    Router::new()
        .route("/", get(root_handler))
        .route("/admin", get(admin_handler))
        .route("/overlay", get(overlay_handler))
        .route("/ws/subtitles", get(ws_subtitles))
        .route("/admin-assets/*asset", get(embedded_admin_asset))
        .route("/overlay-assets/*asset", get(embedded_overlay_asset))
        .nest("/api", api)
        .nest_service("/bin", ServeDir::new(bin_dir))
        // axum 0.7 catch-all syntax is /*path (must be the final segment).
        .route("/_assets/*path", get(embedded_any_asset))
        // ------------------------------------------------------------------
        // Access control (P0-01). One layer, applied to every route, so a new
        // endpoint cannot be added without a deliberate access decision (see
        // `auth::required_access`, which defaults to Admin).
        //
        // `CorsLayer::permissive()` used to sit here: it echoed
        // `Access-Control-Allow-Origin: *` on every response, which let any web
        // page the user visited read `/api/config` and drive the admin API.
        // There is no legitimate cross-origin client 鈥?the panel and the overlay
        // are served by this same origin 鈥?so no CORS layer is needed at all.
        // ------------------------------------------------------------------
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

/// Reject unauthenticated and cross-origin requests before they reach a handler.
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();

    // 1. Origin. Checked for every request, including static assets: a page on
    //    another origin must not even be able to read our HTML (which carries a
    //    token) or probe our endpoints.
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let (host, port) = state.bind_addr.clone();
    if !auth::origin_allowed(origin.as_deref(), &host, port) {
        warn!(%path, origin = ?origin, "rejected cross-origin request");
        return unauthorized("璺?Origin 璇锋眰琚嫆缁濓紙璇蜂粠鏈満绠＄悊椤佃闂級");
    }

    // 2. Token. Accepted from the `X-SLT-Token` header (admin panel), a
    //    `token` query parameter (OBS Browser Source URLs) or a Bearer header
    //    (scripting). The header is preferred because query strings end up in
    //    logs and history.
    let header_token = req
        .headers()
        .get("x-slt-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            req.headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|s| s.trim().to_string())
        });
    let query_token = req.uri().query().and_then(|q| query_param(q, "token"));
    let presented = header_token.or(query_token);
    let role = presented.as_deref().and_then(|t| state.tokens.role_for(t));

    // 3. Role. The overlay token is read-only; anything that can change
    //    configuration, stop recognition or touch recordings needs the admin
    //    token. The static shell is public (it is what carries the token to the
    //    page) but still passes the Origin check above.
    let required = auth::required_access(&method, &path);
    match (required, role) {
        (Access::Public, _) => {}
        (_, None) => {
            warn!(%path, "rejected request without a valid access token");
            return unauthorized("缂哄皯鎴栭敊璇殑璁块棶浠ょ墝");
        }
        (Access::Overlay, Some(_)) => {}
        (Access::Admin, Some(Role::Admin)) => {}
        (Access::Admin, Some(role)) => {
            warn!(%path, %method, role = role.as_str(), "rejected read-only token on an admin route");
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": "鍙浠ょ墝鏃犳潈璁块棶璇ユ帴鍙ｏ紙淇敼閰嶇疆/鍋滄绠＄嚎/璁板綍鎺ュ彛闇€瑕佺鐞嗕护鐗岋級"
                })),
            )
                .into_response();
        }
    }

    // Let the page handlers inject the caller's own token into the HTML, so the
    // panel and the overlay can authenticate without the token ever appearing
    // in a URL.
    let mut req = req;
    req.extensions_mut().insert(role);
    next.run(req).await
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Token recovery for a panel that was opened without one (see the
// `accessToken()` comment in `admin/app.js`).
//
// The user's OBS dock is a URL they typed once ("瑙嗗浘 鈫?鍋滈潬閮ㄤ欢 鈫?鑷畾涔夋祻瑙堝櫒
// 鍋滈潬閮ㄤ欢" with `http://127.0.0.1:8787/admin`), and that URL stays in
// `user.ini` forever. It cannot carry a token that is only generated later, so
// the panel loads, the server hands it an *empty* token on purpose (see
// `inject_token`), and every `/api/*` call is refused 鈥?a dead panel whose only
// symptom is a cryptic 401.
//
// Handing the token to a loopback *page* is what the `Origin` check already
// refuses for an attacker, with one gap: **DNS rebinding**. An attacker page on
// `http://evil.example` can make that name resolve to 127.0.0.1, so its
// `fetch("http://evil.example:8787/api/auth/local-token")` is same-origin, and
// its `Origin` header 鈥?the one value a page cannot forge 鈥?reads
// `http://evil.example:8787`, which is *not* a loopback origin, so
// `auth::origin_allowed` refuses it above.
//
// The `Host` header closes the remaining case (a non-browser client, or a
// browser form of rebinding that manages a loopback-looking Origin): this
// handler additionally requires the request to address us as the **literal
// loopback authority we bound**, which the name-based rebinding request cannot
// do. No other endpoint is loosened: this is a read-only GET whose only effect
// is to hand out a credential the caller could already read from `tokens.toml`.
// ---------------------------------------------------------------------------

/// Answer with this process's admin token, but only when the request addressed
/// the server as the literal loopback authority it bound.
async fn get_local_token(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    let (host, port) = state.bind_addr.clone();
    if !crate::auth::is_loopback_host(&host) {
        // Bound to a LAN address: the page at that address is reachable from
        // other machines, so "the caller is local" is not known here.
        return unauthorized("璇ユ帴鍙ｅ彧鍦ㄧ粦瀹氬洖鐜湴鍧€鏃跺彲鐢紙鏈満绠＄悊椤碉級");
    }
    let authority = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !crate::auth::is_loopback_authority(authority, port) {
        warn!(%authority, path = %uri.path(), "refused to hand out the admin token");
        return unauthorized("Local token recovery is restricted to the bound loopback address");
    }
    let token = state.tokens.admin.clone();
    // Same body as the other token-carrying responses (`obs_dock_url`), so the
    // panel's "鎵撳紑甯︿护鐗岀殑绠＄悊椤? button has one shape to handle.
    axum::Json(serde_json::json!({
        "token": token,
        "admin_url": format!("http://{host}:{port}/admin?token={token}"),
    }))
    .into_response()
}

/// Percent-decoded lookup of one query parameter.
fn query_param(query: &str, want: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == want {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Minimal percent-decoding: tokens are hex, but a hand-edited URL may percent
/// encode them, and a `+` is not a space in a query string value here.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn root_handler() -> &'static str {
    "stream-live-translate is running. Visit /admin to configure, /overlay for the browser source."
}

async fn admin_handler(
    State(state): State<Arc<AppState>>,
    role: Option<axum::Extension<Option<Role>>>,
) -> Response {
    serve_static(&state, "admin/index.html", "text/html; charset=utf-8", role).await
}

async fn overlay_handler(
    State(state): State<Arc<AppState>>,
    role: Option<axum::Extension<Option<Role>>>,
) -> Response {
    serve_static(
        &state,
        "overlay/index.html",
        "text/html; charset=utf-8",
        role,
    )
    .await
}

fn detect_content_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else if lower.ends_with(".wasm") {
        "application/wasm"
    } else if lower.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Token for the role that made this request, or `""` when the caller was
/// anonymous. Used to inject the placeholder into an HTML page.
fn token_for(state: &AppState, role: Option<axum::Extension<Option<Role>>>) -> String {
    match role.and_then(|axum::Extension(r)| r) {
        Some(Role::Admin) => state.tokens.admin.clone(),
        Some(Role::Overlay) => state.tokens.overlay.clone(),
        None => String::new(),
    }
}

/// Inject the caller's own token into an HTML page.
///
/// The page gets the token as a `<script>` global, not as a URL parameter, so it
/// never ends up in browser history, a Referer header, or an access log. An
/// anonymous request gets an empty token, which authenticates nothing.
fn inject_token(bytes: Vec<u8>, token: &str) -> Vec<u8> {
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    if !text.contains(TOKEN_PLACEHOLDER) {
        return text.into_bytes();
    }
    // The token is hex, so no escaping is needed; still, refuse to inject
    // anything that could break out of the string literal.
    //
    // Only the *quoted value* is substituted, never the identifier. A blanket
    // `str::replace` over the whole document also rewrites the variable name
    // (`window.__SLT_TOKEN__ = ...`), which is harmless for a real hex token (it
    // cannot contain `_`) but is a second, needless substitution 鈥?and it breaks
    // silently the moment a token is ever padded or tagged.
    let safe: String = token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    text.replace(&format!("\"{TOKEN_PLACEHOLDER}\""), &format!("\"{safe}\""))
        .into_bytes()
}

async fn serve_static(
    state: &AppState,
    rel: &str,
    content_type: &'static str,
    role: Option<axum::Extension<Option<Role>>>,
) -> Response {
    let token = token_for(state, role);
    // A traversal attempt is a 404, not a 403: nothing about the server's layout
    // is worth confirming to a caller that never had a legitimate path.
    if safe_asset_path(rel).is_none() {
        warn!(%rel, "rejected static asset path");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let candidates = [
        std::path::PathBuf::from(rel),
        std::path::PathBuf::from("dist").join(rel),
    ];
    for c in &candidates {
        if let Ok(data) = std::fs::read(c) {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
                inject_token(data, &token),
            )
                .into_response();
        }
    }
    // Fall back to embedded asset.
    if let Some(bytes) = embedded::read(rel) {
        let ctype = HeaderValue::from_static(detect_content_type(rel));
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, ctype)],
            inject_token(bytes.to_vec(), &token),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, format!("not found: {rel}")).into_response()
}

async fn embedded_admin_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let rel = format!("admin/{path}");
    respond_embedded(&rel)
}

async fn embedded_overlay_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    let rel = format!("overlay/{path}");
    respond_embedded(&rel)
}

async fn embedded_any_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    respond_embedded(&path)
}

/// Reject a static-asset path that tries to leave the directory it belongs to.
///
/// The asset routes became reachable without a token (the browser cannot send a
/// custom header on the top-level navigation that loads the panel), so this is
/// the boundary that matters. A single `..` component, a leading separator, a
/// Windows drive prefix or a NUL is enough to try to read a file outside
/// `admin/`, `overlay/` or `dist/` 鈥?e.g. `/admin-assets/../../../config.toml`,
/// which holds the API key.
fn safe_asset_path(rel: &str) -> Option<&str> {
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return None;
    }
    let bytes = rel.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return None; // `C:...` 鈥?a drive-relative or absolute path.
    }
    for component in std::path::Path::new(rel).components() {
        match component {
            std::path::Component::Normal(_) => {}
            // Everything else (`..`, `.`, `/`, a drive prefix) is refused.
            _ => return None,
        }
    }
    Some(rel)
}

fn respond_embedded(rel: &str) -> Response {
    let Some(rel) = safe_asset_path(rel) else {
        warn!(%rel, "rejected embedded asset path");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if let Some(bytes) = embedded::read(rel) {
        let ctype = HeaderValue::from_static(detect_content_type(rel));
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, ctype)],
            bytes.to_vec(),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, format!("not found: {rel}")).into_response()
}

// ---------------------------------------------------------------------------
// Configuration responses (P0-03)
//
// The GET response is built field by field from an explicit struct, never by
// serialising `Config` and patching the secrets out afterwards. Two reasons:
//   * a new secret-shaped field added to `Config` is then invisible to the API
//     by default, instead of leaking until someone remembers to redact it;
//   * the redaction cannot be defeated by a serialisation detail (a nested
//     table, an `Option`, a custom `Serialize` impl).
//
// `llm.api_key` and `obs.password` are therefore *absent* from every response
// body; only the booleans `api_key_set` / `password_set` are reported.
// ---------------------------------------------------------------------------

/// What `/api/config` returns for `[llm]`.
#[derive(serde::Serialize)]
struct LlmView {
    provider: String,
    model: String,
    endpoint: Option<String>,
    target_lang: String,
    translate_chinese: bool,
    system_prompt: Option<String>,
    segment_ms: u64,
    transcribe: bool,
    transcription_model: String,
    gateway_text: bool,
    workspace_id: String,
    speech_noise_threshold: f32,
    speech_noise_threshold_clamped: bool,
    semantic_punctuation_enabled: bool,
    hotwords: Vec<String>,
    /// Never the key itself.
    api_key: &'static str,
    api_key_set: bool,
}

#[derive(serde::Serialize)]
struct AudioView {
    mode: String,
    use_screen_capture_kit: bool,
    device: String,
    sample_rate: u32,
    channels: u16,
    ingest_port: u16,
}

#[derive(serde::Serialize)]
struct FilterView {
    silence_rms: f32,
    music_spectral_flatness: f32,
    min_segment_ms: u32,
    max_segment_ms: u32,
}

/// What `/api/config` returns for `[obs]`. The password is reported as presence
/// only, exactly like the model key.
#[derive(serde::Serialize)]
struct ObsView {
    auto_connect: bool,
    host: String,
    port: u16,
    register_dock: bool,
    #[serde(skip_serializing)]
    password: (),
    password_set: bool,
}

#[derive(serde::Serialize)]
struct OverlayView {
    font_family: String,
    font_size: u32,
    font_color: String,
    background_color: String,
    background_opacity: f32,
    bg_width: u32,
    bg_height: u32,
    border_radius: u32,
    bg_opacity: u32,
    max_lines: u32,
    display_delay_ms: u64,
    clear_after_ms: u64,
    position: String,
    layout: String,
    animation: String,
    mirror_to_text_source: bool,
}

#[derive(serde::Serialize)]
struct ServerView {
    host: String,
    port: u16,
}

/// The whole `/api/config` body.
#[derive(serde::Serialize)]
struct ConfigView {
    server: ServerView,
    llm: LlmView,
    audio: AudioView,
    filter: FilterView,
    obs: ObsView,
    overlay: OverlayView,
    recording_dir: String,
    /// P1-06: whether finished subtitles are written to this session's JSONL.
    /// The panel shows this as an explicit switch; the audit requires automatic
    /// persistence to be a stated policy, not an implicit one.
    auto_persist: bool,
    /// P2-07: retention in days. `0` = keep forever, which is the shipped
    /// default (deleting evidence by default is a product decision the audit
    /// defers).
    retention_days: u64,
    audio_test: AudioTestView,
    hotword_status: serde_json::Value,
    /// P0-02: set when saving dropped a key because the endpoint's trust domain
    /// changed. The panel surfaces it so the user knows to re-enter the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_dropped: Option<String>,
}

#[derive(serde::Serialize)]
struct AudioTestView {
    quiet_ms: u64,
    speech_ms: u64,
}

/// Build the redacted view of a config. `dropped` explains a key withdrawal.
fn config_view(state: &AppState, cfg: &Config, dropped: Option<String>) -> ConfigView {
    let clamped = state
        .speech_noise_threshold_clamped
        .load(std::sync::atomic::Ordering::Relaxed);
    ConfigView {
        server: ServerView {
            host: cfg.server.host.clone(),
            port: cfg.server.port,
        },
        llm: LlmView {
            provider: cfg.llm.provider.clone(),
            model: cfg.llm.model.clone(),
            endpoint: cfg.llm.endpoint.clone(),
            target_lang: cfg.llm.target_lang.clone(),
            translate_chinese: cfg.llm.translate_chinese,
            system_prompt: cfg.llm.system_prompt.clone(),
            segment_ms: cfg.llm.segment_ms,
            transcribe: cfg.llm.transcribe,
            transcription_model: cfg.llm.transcription_model.clone(),
            gateway_text: cfg.llm.gateway_text,
            workspace_id: cfg.llm.workspace_id.clone(),
            // The value the cloud will really receive, not a hand-edited one.
            speech_noise_threshold: crate::config::clamp_speech_noise_threshold(
                cfg.llm.speech_noise_threshold,
            ),
            speech_noise_threshold_clamped: clamped,
            semantic_punctuation_enabled: cfg.llm.semantic_punctuation_enabled,
            hotwords: cfg.llm.hotwords.clone(),
            api_key: "",
            api_key_set: !cfg.llm.api_key.is_empty(),
        },
        audio: AudioView {
            mode: cfg.audio.mode.clone(),
            use_screen_capture_kit: cfg.audio.use_screen_capture_kit,
            device: cfg.audio.device.clone(),
            sample_rate: cfg.audio.sample_rate,
            channels: cfg.audio.channels,
            ingest_port: cfg.audio.ingest_port,
        },
        filter: FilterView {
            silence_rms: cfg.filter.silence_rms,
            music_spectral_flatness: cfg.filter.music_spectral_flatness,
            min_segment_ms: cfg.filter.min_segment_ms,
            max_segment_ms: cfg.filter.max_segment_ms,
        },
        obs: ObsView {
            auto_connect: cfg.obs.auto_connect,
            host: cfg.obs.host.clone(),
            port: cfg.obs.port,
            register_dock: cfg.obs.register_dock,
            password: (),
            password_set: !cfg.obs.password.is_empty(),
        },
        overlay: OverlayView {
            font_family: cfg.overlay.font_family.clone(),
            font_size: cfg.overlay.font_size,
            font_color: cfg.overlay.font_color.clone(),
            background_color: cfg.overlay.background_color.clone(),
            background_opacity: cfg.overlay.background_opacity,
            bg_width: cfg.overlay.bg_width,
            bg_height: cfg.overlay.bg_height,
            border_radius: cfg.overlay.border_radius,
            bg_opacity: cfg.overlay.bg_opacity,
            max_lines: cfg.overlay.max_lines,
            display_delay_ms: crate::config::clamp_display_delay_ms(Some(
                cfg.overlay.display_delay_ms,
            )),
            clear_after_ms: crate::config::clamp_clear_after_ms(Some(cfg.overlay.clear_after_ms)),
            position: cfg.overlay.position.clone(),
            layout: cfg.overlay.layout.clone(),
            animation: cfg.overlay.animation.clone(),
            mirror_to_text_source: cfg.overlay.mirror_to_text_source,
        },
        recording_dir: cfg.recording_dir.clone(),
        auto_persist: cfg.auto_persist,
        retention_days: cfg.retention_days,
        audio_test: AudioTestView {
            quiet_ms: cfg.audio_test.quiet_ms,
            speech_ms: cfg.audio_test.speech_ms,
        },
        hotword_status: hotword_view(state),
        api_key_dropped: dropped,
    }
}

async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config.read().clone();
    axum::Json(config_view(&state, &cfg, None)).into_response()
}

async fn post_config(
    State(state): State<Arc<AppState>>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Response {
    // Every mutation is computed on a copy first and validated before anything
    // is stored or written, so a rejected request leaves no trace (P0-04).
    let write_guard = state.config_write_lock.lock();
    let current = state.config.read().clone();
    let mut cfg = current.clone();

    let payload_key = payload.pointer("/llm/api_key").and_then(|v| v.as_str());
    if let Err(e) = merge_json(&mut cfg, &payload) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // An empty api_key in the payload means "keep the saved one" (the panel never
    // re-displays a saved secret, so it cannot echo it). This is applied after the
    // trust-domain decision below, which is what compares the two.
    if payload_key == Some("") {
        cfg.llm.api_key = current.llm.api_key.clone();
    }
    // An empty OBS password likewise means "keep it"; clearing is a deliberate
    // separate action.
    if payload.pointer("/obs/password").and_then(|v| v.as_str()) == Some("") {
        cfg.obs.password = current.obs.password.clone();
    }

    // ---- P0-02: the outbound trust boundary -----------------------------
    //
    // The panel always re-submits the whole form, so a save that changes the
    // endpoint would otherwise carry the *saved* key to the new host. A key is
    // only ever valid for the endpoint it was entered against.
    //
    // The decision is made **before** applying the patch: once `api_key` has been
    // overwritten we can no longer tell "the user typed a new key" from "the form
    // echoed the saved one back". The comparison is against the *incoming* value
    // and the *new* provider/endpoint, never against current state 鈥?otherwise a
    // field-ordering fluke would turn the refusal into a silent key reset.
    let old_domain = crate::llm::trust_domain(&current.llm);
    let new_domain = crate::llm::trust_domain(&cfg.llm);
    // The raw value the caller sent, captured before the "keep the saved key"
    // rule below overwrites `cfg.llm.api_key` with the stored secret.
    let incoming_key = payload
        .pointer("/llm/api_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Empty incoming key = "keep the saved one", which is exactly the value the
    // form echoes back. Both spellings mean "carry the existing key forward".
    if let Some(new_domain) = trust_change_drops_key(
        old_domain.as_deref(),
        new_domain.as_deref(),
        &current.llm.api_key,
        incoming_key,
    ) {
        info!(
            from = ?old_domain,
            to = ?new_domain,
            "endpoint trust domain changed: dropping the saved API key"
        );
        // Clear it in the **live state too**, not just on this local copy.
        // Returning 409 without doing so would leave the engine holding a key for
        // an endpoint it is no longer configured to use 鈥?and if the process then
        // restarted or the pending patch were applied by any other path, that key
        // would be sent to the new host. The whole point is that it does not
        // survive.
        {
            let mut live = state.config.write();
            live.llm.api_key.clear();
            live.llm.endpoint = None;
        }
        *state.llm_key_domain.lock() = new_domain.map(str::to_string);
        let message = trust_change_message(old_domain.as_deref(), new_domain);
        let view = config_view(&state, &state.config.read(), Some(message.clone()));
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": message,
                "api_key_dropped": message,
                "config": view,
            })),
        )
            .into_response();
    }
    if !cfg.llm.api_key.is_empty() && old_domain != new_domain {
        // A fresh key typed by the user for the new endpoint: adopt it and
        // record the new trust domain.
        *state.llm_key_domain.lock() = new_domain.clone();
    }

    // ---- P0-04: normalise and validate before anything takes effect ------
    if let Err(errors) = cfg.normalise(&state.config_path) {
        warn!(errors = %errors, "rejected config patch");
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": format!("閰嶇疆琚嫆缁濓細{errors}"),
                "errors": errors.0,
            })),
        )
            .into_response();
    }
    // P0-02: refuse an endpoint the provider may not send credentials to. Checked
    // here so the panel learns immediately, and again in `llm::build()` /
    // `llm::test_connection()` so a hand-edited config.toml cannot bypass it.
    if let Err(e) = crate::llm::check_endpoint_allowed(&cfg.llm) {
        warn!(error = %format!("{e:#}"), "rejected config patch: endpoint not allowed");
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": format!("{e:#}") })),
        )
            .into_response();
    }

    // The cloud's noise threshold is not clamped in the response the same way,
    // so record whether this save had to clamp it (drives the panel's warning).
    {
        let raw = cfg.llm.speech_noise_threshold;
        let (clamped, was_clamped) = crate::config::clamp_speech_noise_threshold_flagged(raw);
        if was_clamped {
            info!(
                from = raw,
                to = clamped,
                "speech_noise_threshold out of range; clamped to [-1.0, 1.0]"
            );
            cfg.llm.speech_noise_threshold = clamped;
        }
        state
            .speech_noise_threshold_clamped
            .store(was_clamped, std::sync::atomic::Ordering::Relaxed);
    }

    // The OBS plugin launches the engine with --audio-mode obs_filter;
    // the audio feed comes from the plugin itself. Never let a panel
    // save silently switch the mode (that kills the pipeline).
    if let Some(forced) = &state.forced_audio_mode {
        if cfg.audio.mode != *forced {
            info!(
                from = %cfg.audio.mode,
                to = %forced,
                "audio mode locked by CLI override; ignoring patch value"
            );
            cfg.audio.mode = forced.clone();
        }
    }

    // Capture the final word list before persisting the configuration.
    let hotwords = cfg.llm.hotwords.clone();
    // P2-02: atomic, serialised save. One lock covers write + read-back +
    // publish, so two concurrent saves cannot interleave into a lost update.
    let saved = {
        if let Err(e) = cfg.save(&state.config_path) {
            // Do NOT pretend success: the admin panel must surface disk-write
            // failures, otherwise users believe their settings were persisted.
            warn!(error = %e, "save config");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": format!("config saved to memory but FAILED to write {}: {e}", state.config_path.display())
                })),
            )
                .into_response();
        }
        // Write-verify: even a successful write can be silently reverted by
        // security/sync software, or land in a redirected location. Read the
        // file back and compare the critical fields so the panel can warn the
        // user that the value will NOT survive an OBS restart.
        let verified = std::fs::read_to_string(&state.config_path)
            .ok()
            .and_then(|raw| toml::from_str::<crate::config::Config>(&raw).ok())
            .map(|disk| {
                disk.llm.api_key == cfg.llm.api_key
                    && disk.llm.model == cfg.llm.model
                    && disk.llm.provider == cfg.llm.provider
                    && disk.server.host == cfg.server.host
                    && disk.server.port == cfg.server.port
            })
            .unwrap_or(false);
        if verified {
            *state.config.write() = cfg.clone();
            true
        } else {
            false
        }
    };
    if !saved {
        warn!(path = %state.config_path.display(), "config write verification FAILED: disk content differs from what was just saved");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": format!(
                    "Configuration is active in memory, but the disk write could not be verified: {}",
                    state.config_path.display()
                )
            })),
        )
            .into_response();
    }
    // 鐑瘝锛圧10锛夛細绔嬪埢鎺ㄨ繘杩愯鏈?feed銆傛鍦ㄨ窇鐨勭櫨鐐间細璇濅細鐢?continue-task
    // 鏀跺埌鏂拌瘝琛紝鍏抽敭璇嶅彉鍖栦笉绠?闇€瑕侀噸鍚?鐨勯厤缃彉鍖栵紙瑙?pipeline::watch锛夛紝
    // Publish changes to the active recognition session without interrupting it.
    state.hotwords.set(crate::hotwords::plan(&hotwords));
    // Tell every connected overlay to re-apply the (now updated) style.
    // Without this the OBS browser source keeps the style it fetched at
    // startup, so the user would have to copy a new URL after every save.
    // An empty payload is intentional: receivers re-read state.config.
    let _ = state.config_tx.send(());
    // Restart the pipeline so it picks up the new config immediately
    // instead of waiting for the next watch() tick. A provider that is still
    // draining the last sentence of a finite replay gets a short grace period
    // so saving settings cannot cut the final subtitle short.
    //
    // P1-01: this is the single reliable restart path. `watch()` is a safety
    // net, not the mechanism 鈥?a save must not depend on a field being in some
    // whitelist for the change to take effect.
    drop(write_guard);
    state
        .pipeline
        .restart_graceful(SETTINGS_RESTART_GRACE)
        .await;
    (StatusCode::OK, axum::Json(serde_json::json!({"ok": true}))).into_response()
}

async fn clear_api_key(State(state): State<Arc<AppState>>) -> Response {
    let write_guard = state.config_write_lock.lock();
    let mut cfg = state.config.read().clone();
    cfg.llm.api_key.clear();
    if let Err(error) = cfg.save(&state.config_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("清除 API Key 写入失败：{error}")})),
        )
            .into_response();
    }
    let verified = std::fs::read_to_string(&state.config_path)
        .ok()
        .and_then(|raw| toml::from_str::<crate::config::Config>(&raw).ok())
        .map(|disk| disk.llm.api_key.is_empty())
        .unwrap_or(false);
    if !verified {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error":"API key clear could not be verified on disk"})),
        )
            .into_response();
    }
    *state.config.write() = cfg;
    drop(write_guard);
    *state.llm_key_domain.lock() = None;
    let _ = state.config_tx.send(());
    state
        .pipeline
        .restart_graceful(SETTINGS_RESTART_GRACE)
        .await;
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

async fn clear_obs_password(State(state): State<Arc<AppState>>) -> Response {
    let write_guard = state.config_write_lock.lock();
    let mut cfg = state.config.read().clone();
    cfg.obs.password.clear();
    if let Err(error) = cfg.save(&state.config_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("清除 OBS 密码写入失败：{error}")})),
        )
            .into_response();
    }
    let verified = std::fs::read_to_string(&state.config_path)
        .ok()
        .and_then(|raw| toml::from_str::<crate::config::Config>(&raw).ok())
        .is_some_and(|disk| disk.obs.password.is_empty());
    if !verified {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error":"清除 OBS 密码后，磁盘校验失败"})),
        )
            .into_response();
    }
    *state.config.write() = cfg;
    let _ = state.config_tx.send(());
    drop(write_guard);
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// Decide whether changing the endpoint must withdraw the saved API key.
///
/// Returns `Some(new_domain)` when the key must be dropped, `None` when the save
/// may proceed with the key as-is. Pure and separate from the handler so the
/// rule is testable without an HTTP round trip 鈥?the audit's exfiltration chain
/// ("change only the endpoint, reuse the stored key") is decided right here.
///
/// `incoming` is the raw `llm.api_key` value from the request, captured **before**
/// the "empty means keep the saved one" rule rewrites it, because after that
/// rewrite the two spellings are indistinguishable.
fn trust_change_drops_key<'a>(
    old_domain: Option<&str>,
    new_domain: Option<&'a str>,
    saved_key: &str,
    incoming: &str,
) -> Option<Option<&'a str>> {
    if old_domain == new_domain {
        // Same host: the key is still scoped to the endpoint it was entered for.
        return None;
    }
    // "Carry the saved key forward" is either an empty field (the panel never
    // re-displays a secret, so it cannot echo it) or the identical value echoed
    // back by a client that does hold it. A *different* non-empty value is the
    // user explicitly entering a key for the new endpoint and is allowed.
    let carries_saved_key = !saved_key.is_empty() && (incoming.is_empty() || incoming == saved_key);
    if carries_saved_key {
        Some(new_domain)
    } else {
        None
    }
}

/// The user-facing explanation for a withdrawn key.
fn trust_change_message(old_domain: Option<&str>, new_domain: Option<&str>) -> String {
    format!(
        "识别端点已从 {} 改为 {}；已保存的 API Key 不再自动沿用。请在新端点下重新输入 API Key。",
        old_domain.unwrap_or("锛堥粯璁わ級"),
        new_domain.unwrap_or("锛堥粯璁わ級"),
    )
}

fn merge_json(cfg: &mut Config, patch: &serde_json::Value) -> anyhow::Result<()> {
    let mut current = serde_json::to_value(cfg.clone())?;
    json_merge(&mut current, patch);
    *cfg = serde_json::from_value(current)?;
    Ok(())
}

use serde_json::Value;

fn json_merge(dst: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value::Object;
    if let (Object(d), Object(p)) = (&mut *dst, patch) {
        for (k, v) in p {
            if v.is_null() {
                d.remove(k);
            } else {
                json_merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else if !patch.is_null() {
        *dst = patch.clone();
    }
}

async fn get_devices() -> Response {
    match crate::audio::list_devices() {
        Ok(devs) => axum::Json(devs).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Host OS UI language, normalised to the two languages the panel ships
/// with ("zh" | "en"). The panel uses this to pick its language with no
/// user-facing toggle 鈥?an English Windows/macOS/Linux gets English.
///
/// Falls back to English for anything unrecognised, which is also what a
/// headless/container host (no locale set) should get.
fn ui_language() -> &'static str {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh",
        _ => "en",
    }
}

async fn get_locale() -> Response {
    axum::Json(serde_json::json!({ "language": ui_language() })).into_response()
}

async fn get_recording_info(State(state): State<Arc<AppState>>) -> Response {
    axum::Json(state.recording.info()).into_response()
}

/// Erase this session's on-disk recording.
///
/// Deliberately separate from銆屾竻绌哄巻鍙层€?`/api/subtitles/history/clear`), which
/// only empties the server-side subtitle list: the audit requires the two to be
/// distinguishable in the UI, because deleting evidence is not the same action
/// as clearing a display buffer (P1-06).
async fn delete_recordings(State(state): State<Arc<AppState>>) -> Response {
    match state.recording.delete_disk() {
        Ok(info) => {
            info!(path = %info.jsonl_path, "recording deleted at the user's request");
            axum::Json(serde_json::json!({
                "ok": true,
                "deleted": info.jsonl_path,
                "message": "本地录音已删除，字幕历史不受影响"
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": format!("删除本地录音失败：{e}")
            })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export_recording(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ExportQuery>,
) -> Response {
    let (srt, content_type) = match query.format.as_deref().unwrap_or("txt") {
        "txt" => (false, HeaderValue::from_static("text/plain; charset=utf-8")),
        "srt" => (
            true,
            HeaderValue::from_static("application/x-subrip; charset=utf-8"),
        ),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error":"format must be txt or srt"})),
            )
                .into_response()
        }
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let recording = state.recording.clone();
    tokio::task::spawn_blocking(move || {
        let mut writer = std::io::BufWriter::new(ExportWriter { tx });
        let result = if srt {
            recording.write_srt(&mut writer)
        } else {
            recording.write_txt(&mut writer)
        };
        if let Err(error) = result.and_then(|_| std::io::Write::flush(&mut writer)) {
            warn!(%error, "recording export ended early");
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)), rx))
    });
    (
        [(header::CONTENT_TYPE, content_type)],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

struct ExportWriter {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl std::io::Write for ExportWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.tx
            .blocking_send(bytes.to_vec())
            .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct StatusView {
    running: bool,
    audio_active: bool,
    input_level: f32,
    llm_connected: bool,
    obs_connected: bool,
    last_error: Option<String>,
    /// Why the OBS WebSocket is not connected (e.g. OBS not running or
    /// the WebSocket server not enabled / wrong password).
    obs_error: Option<String>,
    last_subtitle_at: Option<chrono::DateTime<chrono::Utc>>,
    bind_url: String,
    obs_dock_url: Option<String>,
    /// Ready-to-paste OBS Browser Source URL for the overlay, carrying the
    /// **read-only** token. The panel shows this instead of assembling the URL
    /// itself, so the admin token can never end up in a source that a viewer
    /// might see.
    overlay_url: String,
    /// P0-02: the trust domain the saved API key may be sent to. Shown in the
    /// panel next to the key field so the user can see the key's scope.
    llm_key_domain: Option<String>,
    /// Where the config is persisted; shown in the admin panel so users can
    /// verify their settings were actually saved.
    config_path: String,
    /// Set when the engine was launched with --audio-mode (plugin mode);
    /// the panel then locks the audio mode selector.
    audio_mode_forced: Option<String>,
    /// Guided microphone test timings, so the panel's button label, hint and
    /// local countdown match what the server will actually do.
    audio_test_quiet_ms: u64,
    audio_test_speech_ms: u64,
    /// Effective speech noise threshold after clamping.
    speech_noise_threshold: f32,
    speech_noise_threshold_clamped: bool,
    /// 鐑瘝锛圧10锛変笅鍙戠姸鎬侊細绠＄悊椤电敤瀹冩樉绀?宸蹭笅鍙?N 涓儹璇?/ 涓婃鐢熸晥鏃堕棿 /
    /// Hotword delivery state shown in the admin panel.
    hotwords: serde_json::Value,
}

/// Hotword plan and delivery state, without secrets.
fn hotword_view(state: &AppState) -> serde_json::Value {
    let plan = crate::hotwords::plan(&state.config.read().llm.hotwords);
    let status = state.hotword_status.read().clone();
    serde_json::json!({
        "count": plan.words.len(),
        "rounds": plan.rounds.len(),
        "round_texts": plan.rounds,
        "round_max_chars": crate::hotwords::ROUND_TEXT_MAX_CHARS,
        "max_rounds": crate::hotwords::MAX_ROUNDS,
        "dropped_rounds": plan.dropped_rounds,
        "warnings": plan
            .warnings
            .iter()
            .map(|w| serde_json::json!({"word": w.word, "message": w.message}))
            .collect::<Vec<_>>(),
        "delivered": status.delivered,
        "delivered_count": status.word_count,
        "delivered_rounds": status.round_count,
        "delivered_dropped_rounds": status.dropped_rounds,
        "mode": status.mode,
        "last_applied_at": status.last_applied_at,
        "skipped_unchanged": status.skipped_unchanged,
        "last_result": status.last_result,
        "provider": state.config.read().llm.provider,
    })
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    role: Option<axum::Extension<Option<Role>>>,
) -> Response {
    let cfg = state.config.read().clone();
    let s = state.status.read().clone();
    let (quiet_dur, speech_dur) = cfg.audio_test.durations();
    let noise_threshold =
        crate::config::clamp_speech_noise_threshold(cfg.llm.speech_noise_threshold);
    let noise_threshold_clamped = state
        .speech_noise_threshold_clamped
        .load(std::sync::atomic::Ordering::Relaxed);
    let view = StatusView {
        running: state.pipeline.is_running(),
        audio_active: s.audio_active,
        input_level: s.input_level,
        llm_connected: s.llm_connected,
        obs_connected: s.obs_connected,
        last_error: s.last_error,
        obs_error: s.obs_error,
        last_subtitle_at: s.last_subtitle_at,
        bind_url: format!("http://{}:{}", cfg.server.host, cfg.server.port),
        // The in-OBS dock hosts the admin panel, so it needs the admin token.
        obs_dock_url: if cfg.obs.register_dock
            && matches!(role, Some(axum::Extension(Some(Role::Admin))))
        {
            Some(format!(
                "http://{}:{}/admin?obsDock=1&token={}",
                cfg.server.host, cfg.server.port, state.tokens.admin
            ))
        } else {
            None
        },
        overlay_url: format!(
            "http://{}:{}/overlay?token={}",
            cfg.server.host, cfg.server.port, state.tokens.overlay
        ),
        llm_key_domain: crate::llm::trust_domain(&cfg.llm),
        config_path: state.config_path.display().to_string(),
        audio_mode_forced: state.forced_audio_mode.clone(),
        audio_test_quiet_ms: quiet_dur.as_millis() as u64,
        audio_test_speech_ms: speech_dur.as_millis() as u64,
        speech_noise_threshold: noise_threshold,
        speech_noise_threshold_clamped: noise_threshold_clamped,
        hotwords: hotword_view(&state),
    };
    axum::Json(view).into_response()
}

async fn test_connection(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config.read().llm.clone();
    match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::llm::test_connection(&cfg),
    )
    .await
    {
        Ok(Ok(())) => {
            axum::Json(serde_json::json!({"ok": true, "message": "宸查獙璇侀壌鏉冦€佷换鍔″惎鍔ㄥ拰姝ｅ父缁撴潫"}))
                .into_response()
        }
        Ok(Err(error)) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": format!("连接测试失败：{error:#}")})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            axum::Json(serde_json::json!({"error": "杩炴帴娴嬭瘯瓒呮椂锛岃妫€鏌ョ綉缁溿€佸湴鍩熷拰涓氬姟绌洪棿"})),
        )
            .into_response(),
    }
}

#[derive(Clone, Copy)]
struct MeterSnapshot {
    frames: u64,
    rms_sum: f64,
    peak: f32,
}

fn meter_snapshot(state: &AppState) -> MeterSnapshot {
    let status = state.status.read();
    MeterSnapshot {
        frames: status.input_frames,
        rms_sum: status.input_rms_sum,
        peak: status.input_peak,
    }
}

/// Mean RMS over the frames counted since `previous`, plus the frame count of
/// that window. Pure so the arithmetic behind the guided test is unit-testable
/// without a capture device.
///
/// Returns `None` when `total` holds fewer frames than `previous`, which means
/// the counters were reset mid-test (a pipeline restart or a second concurrent
/// test). Subtracting across that reset produces a meaningless average, so the
/// caller must fail loudly instead of reporting a fabricated level.
fn average_over_window(
    total: &MeterSnapshot,
    previous: Option<&MeterSnapshot>,
) -> Option<(f64, u64)> {
    let (base_frames, base_sum) = previous.map_or((0, 0.0), |p| (p.frames, p.rms_sum));
    if total.frames < base_frames {
        return None;
    }
    let frames = total.frames - base_frames;
    let sum = (total.rms_sum - base_sum).max(0.0);
    Some((
        if frames == 0 {
            0.0
        } else {
            sum / frames as f64
        },
        frames,
    ))
}

/// Advice derived from one guided-test pair of meter snapshots. Split out of
/// the handler so all four branches are covered by unit tests instead of only
/// being reachable with a real microphone attached.
fn audio_test_advice(quiet_avg: f64, speech_avg: f64, peak: f32) -> &'static str {
    if speech_avg < 0.005 {
        "璁茶瘽闊抽噺杩囦綆锛氶潬杩戦害鍏嬮鎴栨彁楂樿緭鍏ュ鐩婂悗閲嶈瘯"
    } else if peak > 0.98 {
        "妫€娴嬪埌鍙兘鍓婃尝锛氶檷浣庤緭鍏ュ鐩婂悗閲嶈瘯"
    } else if speech_avg < quiet_avg * 1.5 {
        "说话声与背景噪声差异较小，请检查麦克风并调整过滤预设"
    } else {
        "杈撳叆淇″彿鍙敤锛涜缁撳悎瀹為檯瀛楀箷鍐嶉€夋嫨杩囨护棰勮"
    }
}

async fn run_audio_test(State(state): State<Arc<AppState>>) -> Response {
    if !state.status.read().audio_active {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "音频输入未运行，请先保存配置并确认管线已启动"})),
        )
            .into_response();
    }
    // Durations come from [audio_test] so a microphone that needs a longer
    // window can be calibrated without editing the binary.
    let (quiet_dur, speech_dur) = state.config.read().audio_test.durations();
    {
        let mut status = state.status.write();
        status.input_frames = 0;
        status.input_rms_sum = 0.0;
        status.input_peak = 0.0;
    }
    tokio::time::sleep(quiet_dur).await;
    let quiet = meter_snapshot(&state);
    tokio::time::sleep(speech_dur).await;
    let spoken = meter_snapshot(&state);
    // A reset inside either window invalidates the pair: report it instead of
    // telling the user their microphone is too quiet.
    let Some((quiet_avg, _)) = average_over_window(&quiet, None) else {
        return (
            StatusCode::CONFLICT,
            axum::Json(
                serde_json::json!({"error": "璇曢煶鏈熼棿闊抽缁熻琚噸缃紙绠＄嚎鍙兘鍒氶噸鍚級锛岃閲嶈瘯" }),
            ),
        )
            .into_response();
    };
    let Some((speech_avg, speech_frames)) = average_over_window(&spoken, Some(&quiet)) else {
        return (
            StatusCode::CONFLICT,
            axum::Json(
                serde_json::json!({"error": "璇曢煶鏈熼棿闊抽缁熻琚噸缃紙绠＄嚎鍙兘鍒氶噸鍚級锛岃閲嶈瘯" }),
            ),
        )
            .into_response();
    };
    if spoken.frames == 0 || speech_frames == 0 {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "鏈敹鍒伴煶棰戯紝璇锋鏌ユ墍閫夐害鍏嬮鎴?OBS 闊虫簮"})),
        )
            .into_response();
    }
    let advice = audio_test_advice(quiet_avg, speech_avg, spoken.peak);
    axum::Json(serde_json::json!({
        "ok": true,
        "quiet_rms": quiet_avg,
        "speech_rms": speech_avg,
        "peak": spoken.peak,
        "quiet_ms": quiet_dur.as_millis() as u64,
        "speech_ms": speech_dur.as_millis() as u64,
        "message": advice,
    }))
    .into_response()
}

#[derive(serde::Serialize)]
struct SubtitlesView {
    current: Option<crate::subtitle::SubtitleLine>,
    history: Vec<crate::subtitle::SubtitleLine>,
}

async fn get_subtitles(State(state): State<Arc<AppState>>) -> Response {
    let view = SubtitlesView {
        current: state.subtitle.current(),
        history: state.subtitle.history(),
    };
    axum::Json(view).into_response()
}

async fn clear_subtitles(State(state): State<Arc<AppState>>) -> Response {
    state.subtitle.clear();
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// Clear the finished-sentence history as well as the open line, so the panel's
/// 銆屾竻绌哄巻鍙层€?button has a backend that matches its label. The recording store
/// behind `/api/recordings/export` is untouched.
async fn clear_subtitle_history(State(state): State<Arc<AppState>>) -> Response {
    state.subtitle.clear_history();
    axum::Json(serde_json::json!({"ok": true, "history": 0})).into_response()
}

async fn restart_pipeline(State(state): State<Arc<AppState>>) -> Response {
    // restart() (NOT shutdown()): the pipeline run-loop must stay alive and
    // spin up a fresh pipeline with the current config. shutdown() is
    // permanent and reserved for process exit.
    //
    // The grace period only applies when a provider session is still alive
    // (draining a finite replay): "閲嶅惎绠＄嚎" must not discard the sentence the
    // cloud is about to hand back.
    //
    // `resume` first: this endpoint is also how a stopped pipeline is started
    // again, so it must lift the pause before the run loop is asked to start.
    state.pipeline.resume().await;
    state
        .pipeline
        .restart_graceful(SETTINGS_RESTART_GRACE)
        .await;
    // Broadcast Cleared to all WebSocket clients so overlays clear their state.
    state.subtitle.clear();
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// Stop the current pipeline run and keep it stopped until `/api/restart`.
///
/// Without a real stop the run loop only ends on process exit, so a stop that
/// merely dropped the inner state was undone by the next `try_start` 鈥?including
/// the 30 s first-audio wait, which nothing could interrupt: `/api/restart` and a
/// config save both answered `{ok:true}` while the previous attempt was still
/// parked in that wait.
async fn stop_pipeline(State(state): State<Arc<AppState>>) -> Response {
    state.pipeline.pause().await;
    state.subtitle.clear();
    {
        let mut s = state.status.write();
        s.audio_active = false;
        s.llm_connected = false;
        s.last_error = Some(crate::pipeline::PIPELINE_STOPPED_MESSAGE.into());
    }
    axum::Json(serde_json::json!({"ok": true, "running": false})).into_response()
}

async fn ws_subtitles(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, state))
}

#[derive(serde::Deserialize, Default)]
struct WsQuery {
    #[serde(default)]
    since: Option<String>,
}

/// Push the current overlay style to one client as `{"type":"config",...}`.
/// Returns false when the socket died and the caller should stop the loop.
async fn send_config(socket: &mut WebSocket, state: &Arc<AppState>) -> bool {
    let mut ov = state.config.read().overlay.clone();
    // Same clamping as GET /api/config: the overlay enforces this range too, so
    // send the effective value instead of a raw hand-edited one.
    ov.clear_after_ms = crate::config::clamp_clear_after_ms(Some(ov.clear_after_ms));
    // 0 = 銆屾棤缂撳啿銆?is legal, so the delay uses its own rule (0, or 500鈥?000).
    ov.display_delay_ms = crate::config::clamp_display_delay_ms(Some(ov.display_delay_ms));
    let payload = serde_json::json!({
        "type": "config",
        "overlay": ov,
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .is_ok()
}

async fn ws_loop(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.subtitle.subscribe();
    let mut cfg_rx = state.config_tx.subscribe();
    // Style first: a freshly opened browser source (or one that just
    // reconnected) must render correctly even if nothing changes later.
    if !send_config(&mut socket, &state).await {
        return;
    }
    // Send the current state immediately.
    if let Some(cur) = state.subtitle.current() {
        let payload = serde_json::json!({
            "type": "current",
            "line": cur,
        });
        if socket
            .send(Message::Text(payload.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                // Serialize by hand (see subtitle::ws_payload): serde_json
                // cannot serialize the tagged enum directly, and the old
                // `to_string(&ev).unwrap_or_default()` silently shipped an
                // empty message, so overlay/admin never got any text.
                let payload = crate::subtitle::ws_payload(&ev);
                if socket
                    .send(Message::Text(payload.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Overlay style saved from the admin panel: restyle live.
            cfg = cfg_rx.recv() => {
                // Err means the sender is gone (shutting down); bail out
                // instead of spinning on a permanently-ready branch.
                if cfg.is_err() {
                    return;
                }
                if !send_config(&mut socket, &state).await {
                    return;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // Application-level liveness probe from the overlay.
                        // TCP can be half-open (NAT timeout, sleeping machine)
                        // without the browser ever firing `close`, so the
                        // overlay pings and expects *something* back; a silent
                        // peer would otherwise look identical to a dead stream.
                        if text.as_str() == WS_PING_TEXT {
                            if socket.send(Message::Text(WS_PONG_TEXT.into())).await.is_err() {
                                return;
                            }
                        }
                        // Everything else is reserved for future client commands.
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(frames: u64, rms_sum: f64, peak: f32) -> MeterSnapshot {
        MeterSnapshot {
            frames,
            rms_sum,
            peak,
        }
    }

    #[test]
    fn quiet_window_average_uses_all_frames() {
        let total = snap(4, 0.04, 0.02);
        let (avg, frames) =
            average_over_window(&total, None).expect("first window is always valid");
        assert_eq!(frames, 4);
        assert!((avg - 0.01).abs() < 1e-12, "avg was {avg}");
    }

    #[test]
    fn speech_window_subtracts_the_quiet_phase() {
        // 10 frames total: the first 4 belong to the quiet phase.
        let quiet = snap(4, 0.04, 0.02);
        let spoken = snap(10, 0.04 + 0.12, 0.20);
        let (avg, frames) = average_over_window(&spoken, Some(&quiet)).expect("monotonic counters");
        assert_eq!(frames, 6);
        assert!((avg - 0.02).abs() < 1e-12, "avg was {avg}");
    }

    #[test]
    fn a_window_without_new_frames_is_treated_as_silence() {
        let quiet = snap(4, 0.04, 0.02);
        let (avg, frames) = average_over_window(&quiet, Some(&quiet)).expect("zero-length window");
        assert_eq!(frames, 0);
        assert_eq!(avg, 0.0);
    }

    #[test]
    fn counters_that_move_backwards_are_reported_as_a_reset() {
        // The counters are zeroed by the start of every test run and by a
        // pipeline restart, so a shrinking snapshot must be rejected rather
        // than averaged into a fake "too quiet" verdict.
        let previous = snap(10, 2.0, 0.5);
        let current = snap(2, 0.1, 0.1);
        assert!(average_over_window(&current, Some(&previous)).is_none());
    }

    #[test]
    fn advice_covers_too_quiet_clipping_and_insufficient_contrast() {
        assert!(audio_test_advice(0.001, 0.001, 0.05).contains("闊抽噺杩囦綆"));
        assert!(audio_test_advice(0.001, 0.05, 1.0).contains("鍓婃尝"));
        assert!(audio_test_advice(0.02, 0.021, 0.2).contains("差异较小"));
        assert!(audio_test_advice(0.001, 0.05, 0.2).contains("鍙敤"));
    }

    /// POST /api/config 璧扮殑鏄?`merge_json`锛氶潰鏉挎彁浜ょ殑
    /// `llm.speech_noise_threshold` 蹇呴』钀借繘 Config锛屼笖淇濆瓨鍚庣殑 GET 鑳借鍥炪€?    #[test]
    fn a_config_patch_carries_speech_noise_threshold() {
        let mut cfg = Config::default();
        assert_eq!(cfg.llm.speech_noise_threshold, 0.0);
        let patch = serde_json::json!({"llm": {"speech_noise_threshold": 0.9}});
        merge_json(&mut cfg, &patch).expect("merge threshold patch");
        assert_eq!(cfg.llm.speech_noise_threshold, 0.9);
        // 0.0 涔熸槸鍚堟硶鍊硷紝蹇呴』鐪熺殑鍐欏洖 0.0 鑰屼笉鏄褰撴垚"缂虹渷/绌?蹇界暐銆?        let patch = serde_json::json!({"llm": {"speech_noise_threshold": 0.0}});
        merge_json(&mut cfg, &patch).expect("merge zero threshold");
        assert_eq!(cfg.llm.speech_noise_threshold, 0.0);
        let patch = serde_json::json!({"llm": {"speech_noise_threshold": -1.0}});
        merge_json(&mut cfg, &patch).expect("merge negative threshold");
        assert_eq!(cfg.llm.speech_noise_threshold, -1.0);
    }

    /// 瓒婄晫琛ヤ竵浼氳 POST 澶勭悊璺緞閽冲洖 [-1.0, 1.0]锛堝悓涓€濂楅挸鍒跺嚱鏁帮級锛?    /// 鎵€浠ョ鐩橀噷涓嶄細鐣欎笅浜戠浼氭嫆缁濈殑鍊笺€?    #[test]
    fn out_of_range_patches_are_clamped_the_same_way_as_the_save_path() {
        for (raw, expected) in [(5.0_f32, 1.0_f32), (-5.0, -1.0), (0.35, 0.35)] {
            let (clamped, was_clamped) = crate::config::clamp_speech_noise_threshold_flagged(raw);
            assert_eq!(clamped, expected);
            assert_eq!(was_clamped, clamped != raw);
        }
    }

    // -----------------------------------------------------------------------
    // Local hostile-request acceptance tests (P0-01 / P0-02 / P0-03 / P0-04).
    //
    // The audit's regression gate requires proof that the security boundary holds
    // against a *request*, not merely that a helper function returns the right
    // value. These tests therefore start the real `axum::serve` on an ephemeral
    // loopback port and speak real HTTP to it, so a route added without an access
    // decision, a layer removed by accident, or a handler that starts returning a
    // secret all fail here.
    //
    // They never touch the developer's real files: the config path is redirected
    // into a temp directory, and a rejected `POST /api/config` must leave no file
    // behind at all.
    // -----------------------------------------------------------------------

    use std::io::{Read, Write};

    fn test_state(dir: &std::path::Path, port: u16) -> Arc<AppState> {
        let mut cfg = Config::default();
        cfg.recording_dir = String::new();
        let (config_tx, _) = tokio::sync::broadcast::channel::<()>(4);
        let hotwords = crate::hotwords::HotwordFeed::new();
        hotwords.set(crate::hotwords::plan(&cfg.llm.hotwords));
        Arc::new(AppState {
            config: Arc::new(parking_lot::RwLock::new(cfg.clone())),
            config_tx,
            subtitle: Arc::new(crate::subtitle::SubtitleHub::default()),
            pipeline: Arc::new(crate::pipeline::PipelineHandle::new()),
            status: Arc::new(parking_lot::RwLock::new(crate::AppStatus::default())),
            recording: crate::recording::RecordingStore::start(&dir.join("config.toml"), ""),
            obs_cmd_tx: parking_lot::Mutex::new(None),
            forced_audio_mode: None,
            provider_failures: std::sync::atomic::AtomicU32::new(0),
            hotword_status: Arc::new(parking_lot::RwLock::new(
                crate::hotwords::HotwordStatus::default(),
            )),
            hotwords,
            speech_noise_threshold_clamped: std::sync::atomic::AtomicBool::new(false),
            tokens: crate::auth::TokenPair {
                admin: "a".repeat(64),
                overlay: "b".repeat(64),
            },
            bind_addr: ("127.0.0.1".into(), port),
            auth_configured: false,
            llm_key_domain: parking_lot::Mutex::new(None),
            ingest_nonce: None,
            config_write_lock: parking_lot::Mutex::new(()),
            // The temp dir, so a patch's `recording_dir` is validated against the
            // directory the test config really lives in 鈥?which is the whole
            // point of the traversal check. Validating against the `CONFIG_PATH`
            // default (`"config.toml"`, parent `.`) made every absolute path look
            // "inside" and the check silently passed.
            config_path: dir.join("config.toml"),
        })
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("slt-server-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Start the real server on an ephemeral loopback port and return its base
    /// URL together with the state, whose `bind_addr` carries the **real** port.
    ///
    /// The port must be real, not a placeholder: the `Origin` check compares the
    /// caller's port against it, and the `AuthState` is built from the state
    /// before `serve` binds 鈥?so a test that hardcoded 8787 would make the
    /// same-origin assertions pass for the wrong reason.
    async fn start_server(tag: &str) -> (String, Arc<AppState>, std::path::PathBuf) {
        let dir = temp_dir(tag);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = test_state(&dir, port);
        let app = build_router(state.clone(), dir.join("dist"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), state, dir)
    }

    struct HttpResponse {
        status: u16,
        body: String,
    }

    impl HttpResponse {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
        }
    }

    /// One blocking HTTP/1.1 request on its own connection.
    ///
    /// `Connection: close` is sent so the body ends at EOF, which also gives a
    /// free chunked-encoding path: the raw bytes are decoded below rather than
    /// needing a full HTTP client.
    fn http(
        base: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        origin: Option<&str>,
        body: Option<&str>,
    ) -> HttpResponse {
        let addr = base.trim_start_matches("http://").to_string();
        let mut stream = std::net::TcpStream::connect(&addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("read timeout");
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
        if let Some(t) = token {
            req.push_str(&format!("X-SLT-Token: {t}\r\n"));
        }
        if let Some(o) = origin {
            req.push_str(&format!("Origin: {o}\r\n"));
        }
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        stream.write_all(req.as_bytes()).expect("write request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let chunked = head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        HttpResponse {
            status,
            body: if chunked {
                decode_chunked(rest)
            } else {
                rest.to_string()
            },
        }
    }

    /// Minimal `Transfer-Encoding: chunked` decoder for a complete body.
    fn decode_chunked(raw: &str) -> String {
        let mut out = String::new();
        let mut rest = raw;
        loop {
            let Some((size_line, after)) = rest.split_once("\r\n") else {
                break;
            };
            let size = size_line
                .split(';')
                .next()
                .and_then(|hex| usize::from_str_radix(hex.trim(), 16).ok())
                .unwrap_or(0);
            if size == 0 {
                break;
            }
            if after.len() < size {
                out.push_str(after);
                break;
            }
            out.push_str(&after[..size]);
            rest = after[size..].strip_prefix("\r\n").unwrap_or(&after[size..]);
        }
        out
    }

    /// No token at all: refused, for the data paths as well as the write paths.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_without_a_token_are_rejected() {
        let (base, _state, dir) = start_server("notoken").await;
        for (method, path) in [
            ("GET", "/api/config"),
            ("GET", "/api/status"),
            ("GET", "/api/subtitles"),
            ("GET", "/api/recordings"),
            ("GET", "/api/devices"),
            ("POST", "/api/stop"),
            ("POST", "/api/restart"),
            ("GET", "/api/recordings/export?format=txt"),
        ] {
            let response = http(&base, method, path, None, None, None);
            assert_eq!(
                response.status, 401,
                "{method} {path} must require a token (got {})",
                response.status
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wrong token is not a token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_with_a_wrong_token_are_rejected() {
        let (base, _state, dir) = start_server("badtoken").await;
        for token in ["c".repeat(64).as_str(), "a", "", &"A".repeat(64)] {
            let response = http(&base, "GET", "/api/config", Some(token), None, None);
            assert_eq!(response.status, 401, "token {token:?} must be refused");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page on another origin must not be able to reach the API even with a
    /// valid token 鈥?that combination is what a DNS-rebinding attack produces.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cross_origin_request_is_rejected_even_with_a_valid_token() {
        let (base, state, dir) = start_server("origin").await;
        let admin = state.tokens.admin.clone();
        // The ephemeral port the OS actually gave us. The Origin check compares
        // this exact port, so a hardcoded one would make the assertions below
        // pass or fail for the wrong reason.
        let port = base.rsplit(':').next().expect("base url has a port");
        for origin in [
            "http://evil.example".to_string(),
            format!("http://evil.example:{port}"),
            "http://127.0.0.1.example.com".to_string(),
            format!("http://attacker.example:{port}"),
            "null".to_string(),
        ] {
            let response = http(
                &base,
                "GET",
                "/api/config",
                Some(&admin),
                Some(&origin),
                None,
            );
            assert_eq!(
                response.status, 401,
                "cross-origin {origin} must be refused even with a valid token"
            );
        }
        // Our own page 鈥?the panel's Origin 鈥?is accepted, both spellings.
        for same_origin in [
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ] {
            let ok = http(
                &base,
                "GET",
                "/api/config",
                Some(&admin),
                Some(&same_origin),
                None,
            );
            assert_eq!(ok.status, 200, "same-origin {same_origin} must still work");
        }
        // A missing port is a *different* origin (it means :80), not our port.
        let wrong_port = http(
            &base,
            "GET",
            "/api/config",
            Some(&admin),
            Some("http://127.0.0.1"),
            None,
        );
        assert_eq!(
            wrong_port.status, 401,
            "an Origin without a port means :80 and must not be accepted for an ephemeral port"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The read-only overlay token must not be able to change anything, and must
    /// not be able to read the endpoints that expose credentials or recordings.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_overlay_token_cannot_reach_admin_routes() {
        let (base, state, dir) = start_server("overlay").await;
        let overlay = state.tokens.overlay.clone();
        let admin = state.tokens.admin.clone();

        for (method, path) in [
            ("POST", "/api/config"),
            ("POST", "/api/stop"),
            ("POST", "/api/restart"),
            ("POST", "/api/config/clear-key"),
            ("POST", "/api/connection-test"),
            ("GET", "/api/recordings"),
            ("GET", "/api/recordings/export?format=txt"),
            ("GET", "/api/devices"),
        ] {
            let response = http(&base, method, path, Some(&overlay), None, None);
            assert_eq!(
                response.status, 403,
                "{method} {path} must be admin-only (got {})",
                response.status
            );
        }

        // 鈥hile the read paths it genuinely needs still work.
        for path in ["/api/config", "/api/status", "/api/subtitles"] {
            let response = http(&base, "GET", path, Some(&overlay), None, None);
            assert_eq!(response.status, 200, "overlay must be able to GET {path}");
            assert!(
                !response.body.contains(&admin),
                "{path} exposed the admin token to the overlay role"
            );
        }
        // The admin token still reaches everything.
        let response = http(&base, "GET", "/api/recordings", Some(&admin), None, None);
        assert_eq!(response.status, 200);
        let status = http(&base, "GET", "/api/status", Some(&admin), None, None);
        assert!(
            status.body.contains(&admin),
            "only admin status may contain the dock URL token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obs_password_can_be_cleared_without_exposing_it() {
        let (base, state, dir) = start_server("clear-obs-password").await;
        state.config.write().obs.password = "secret".into();
        let admin = state.tokens.admin.clone();
        let response = http(
            &base,
            "POST",
            "/api/config/clear-obs-password",
            Some(&admin),
            None,
            None,
        );
        assert_eq!(response.status, 200);
        assert!(state.config.read().obs.password.is_empty());
        let disk = crate::config::Config::load_or_create(&state.config_path).unwrap();
        assert!(disk.obs.password.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_export_streams_a_response() {
        let (base, state, dir) = start_server("export-stream").await;
        let response = http(
            &base,
            "GET",
            "/api/recordings/export?format=txt",
            Some(&state.tokens.admin),
            None,
            None,
        );
        assert_eq!(response.status, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The audit's exfiltration chain, decided in isolation: change only the
    /// endpoint, reuse the stored key. Every "carry the key forward" spelling must
    /// drop it, and a key the user typed for the new host must be accepted.
    #[test]
    fn the_trust_change_rule_drops_a_carried_key_and_accepts_a_new_one() {
        let saved = "sk-saved-for-dashscope";
        let official = Some("dashscope.aliyuncs.com");
        let other = Some("ws-demo.cn-beijing.maas.aliyuncs.com");

        // Moving to a different host...
        // ...with an empty field (the panel's spelling of "keep it") -> drop.
        assert_eq!(
            trust_change_drops_key(official, other, saved, ""),
            Some(other),
            "an empty field means 'keep the saved key' and must drop it on a host change"
        );
        // ...with the identical value echoed back -> drop.
        assert_eq!(
            trust_change_drops_key(official, other, saved, saved),
            Some(other),
            "an echoed copy of the saved key must also drop on a host change"
        );
        // ...with a key the user typed for the new endpoint -> allowed.
        assert_eq!(
            trust_change_drops_key(official, other, saved, "sk-typed-for-the-new-host"),
            None,
            "a different key is the user deliberately re-entering it"
        );
        // ...with no saved key at all -> nothing to drop.
        assert_eq!(trust_change_drops_key(official, other, "", ""), None);

        // Staying on the same host never drops, whichever spelling is used.
        for incoming in ["", saved, "sk-typed"] {
            assert_eq!(
                trust_change_drops_key(official, official, saved, incoming),
                None,
                "same host with incoming={incoming:?} must keep the key"
            );
        }
        // Moving to "no host at all" (mock) also drops: the key no longer has the
        // scope it was entered for.
        assert_eq!(
            trust_change_drops_key(official, None, saved, ""),
            Some(None)
        );
        // A mock provider has no key to protect.
        assert_eq!(trust_change_drops_key(None, official, "", ""), None);
    }

    /// The message must name both hosts, so a user can tell *why* their key was
    /// rejected without reading the source.
    #[test]
    fn the_trust_change_message_names_both_endpoints() {
        let message = trust_change_message(Some("old.example"), Some("new.example"));
        assert!(message.contains("old.example"), "{message}");
        assert!(message.contains("new.example"), "{message}");
        assert!(message.contains("API Key"), "{message}");
        // Missing domains are described rather than rendered as `None`.
        let message = trust_change_message(None, None);
        assert!(!message.contains("None"), "{message}");
    }

    /// P0-03: no response body may contain the saved model key or OBS password.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_response_contains_a_saved_secret() {
        const MODEL_KEY: &str = "sk-LEAK-CANARY-model-key";
        const OBS_PASSWORD: &str = "LEAK-CANARY-obs-password";
        let (base, state, dir) = start_server("secrets").await;
        {
            let mut cfg = state.config.write();
            cfg.llm.api_key = MODEL_KEY.into();
            cfg.obs.password = OBS_PASSWORD.into();
        }
        let admin = state.tokens.admin.clone();

        for path in [
            "/api/config",
            "/api/status",
            "/api/locale",
            "/api/subtitles",
        ] {
            let response = http(&base, "GET", path, Some(&admin), None, None);
            assert_eq!(response.status, 200, "{path}");
            let raw = response.body.replace("\\/", "/");
            assert!(
                !raw.contains(MODEL_KEY),
                "{path} leaked the model API key: {raw}"
            );
            assert!(
                !raw.contains(OBS_PASSWORD),
                "{path} leaked the OBS password: {raw}"
            );
        }

        // The redaction must be reported as presence, not silently dropped, or
        // the panel cannot tell the user a key exists.
        let view = http(&base, "GET", "/api/config", Some(&admin), None, None).json();
        assert_eq!(view["llm"]["api_key_set"], serde_json::json!(true));
        assert_eq!(view["llm"]["api_key"], serde_json::json!(""));
        assert_eq!(view["obs"]["password_set"], serde_json::json!(true));
        assert!(
            view["obs"].get("password").is_none(),
            "the password field must not exist at all: {view}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The overlay must never be handed the admin secret, and an anonymous page
    /// load must not hand out any token at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_public_shell_only_ever_carries_the_callers_own_token() {
        let (base, state, dir) = start_server("shell").await;
        let admin = state.tokens.admin.clone();
        let overlay = state.tokens.overlay.clone();

        let anonymous = http(&base, "GET", "/overlay", None, None, None);
        assert_eq!(anonymous.status, 200, "the overlay page itself is public");
        assert!(
            !anonymous.body.contains(&admin) && !anonymous.body.contains(&overlay),
            "an anonymous page load must not hand out a real token"
        );
        assert!(
            anonymous.body.contains("window.__SLT_TOKEN__"),
            "the placeholder must still be present for the page to fill in"
        );

        let authenticated = http(&base, "GET", "/overlay", Some(&overlay), None, None);
        assert_eq!(authenticated.status, 200);
        assert!(
            authenticated.body.contains(&overlay),
            "the overlay page must carry the overlay's own token"
        );
        assert!(
            !authenticated.body.contains(&admin),
            "the overlay page must never receive the admin token"
        );

        let panel = http(&base, "GET", "/admin", Some(&admin), None, None);
        assert_eq!(panel.status, 200);
        assert!(
            panel.body.contains(&admin),
            "the panel gets the admin token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-04: values that cannot be normalised must be **rejected with 400 and
    /// write nothing**; values that only need bounding may be clamped instead,
    /// but the clamped result must never exceed the boundary.
    ///
    /// The two are separated deliberately. A blanket "everything is a 400" test
    /// would force clumsy behaviour on the panel (one bad font size should not
    /// refuse an otherwise-fine save), while a blanket "everything is clamped"
    /// test would hide a real refusal. Both directions are asserted, and the
    /// file-write assertion is what makes the refusal meaningful.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hostile_config_patch_is_rejected_and_writes_nothing() {
        let (base, state, dir) = start_server("hostilecfg").await;
        let admin = state.tokens.admin.clone();
        let post = |body: String| {
            http(
                &base,
                "POST",
                "/api/config",
                Some(&admin),
                None,
                Some(&body),
            )
        };

        let before = state.config.read().clone();

        // --- must be refused outright -------------------------------------
        // Each of these is either meaningless, unrepresentable, or would make the
        // pipeline act on a rate/directory it cannot honour.
        for (patch, why) in [
            (
                serde_json::json!({"server": {"port": 0}}),
                "random management port",
            ),
            (
                serde_json::json!({"audio": {"sample_rate": 0}}),
                "no rate at all",
            ),
            (
                serde_json::json!({"audio": {"sample_rate": 1}}),
                "one sample per second",
            ),
            (
                serde_json::json!({"audio": {"sample_rate": 4_294_967_295u32}}),
                "overflows the frame arithmetic",
            ),
            (
                serde_json::json!({"audio": {"sample_rate": 7_999}}),
                "below the window",
            ),
            (
                serde_json::json!({"audio": {"sample_rate": 384_001}}),
                "above the window",
            ),
            (
                serde_json::json!({"audio": {"mode": "not-a-mode"}}),
                "unknown mode",
            ),
            (
                serde_json::json!({"recording_dir": "../../../escape"}),
                "traversal",
            ),
            (
                serde_json::json!({"recording_dir": "\\\\attacker\\share"}),
                "UNC share",
            ),
            (
                serde_json::json!({"recording_dir": "C:\\Windows\\Temp\\slt-escape"}),
                "absolute path outside the config dir",
            ),
            (
                serde_json::json!({"llm": {"model": "x".repeat(5000)}}),
                "oversized string",
            ),
            (
                serde_json::json!({"retention_days": 100_000u64}),
                "absurd retention",
            ),
            (
                serde_json::json!({"llm": {"provider": "qwen-realtime", "endpoint": "wss://evil.example/x"}}),
                "endpoint outside the provider allow-list",
            ),
        ] {
            let response = post(patch.to_string());
            assert_eq!(
                response.status, 400,
                "must be refused ({why}): {patch} -> {} {}",
                response.status, response.body
            );
            assert!(
                !response.body.is_empty(),
                "a refusal must explain itself ({why})"
            );
        }

        // --- may be bounded instead, but never beyond the boundary ---------
        // An oversized hotword is *dropped* rather than refused: a hotword list
        // is best-effort context, so refusing the whole save over one over-long
        // entry would be hostile. The assertion below is what makes "dropped"
        // meaningful 鈥?it must not survive into the state.
        {
            let response =
                post(serde_json::json!({"llm": {"hotwords": ["y".repeat(500)]}}).to_string());
            assert_eq!(
                response.status, 200,
                "an oversized hotword must not refuse the save"
            );
            assert!(
                state
                    .config
                    .read()
                    .llm
                    .hotwords
                    .iter()
                    .all(|w| w.len() <= 64),
                "an over-long hotword must not reach the config"
            );
        }
        let clamped = post(
            serde_json::json!({
                "audio": {"channels": 65_535, "ingest_port": 80},
                "overlay": {"bg_opacity": 5_000, "border_radius": 99_999, "max_lines": 99},
                "filter": {"silence_rms": 5.0},
            })
            .to_string(),
        );
        assert_eq!(
            clamped.status, 200,
            "a silly-but-harmless number must not refuse the whole save: {}",
            clamped.body
        );
        {
            let now = state.config.read();
            assert!(
                now.audio.channels <= 2,
                "channels was {}",
                now.audio.channels
            );
            assert!(
                now.audio.ingest_port >= 1_024,
                "ingest_port was {}",
                now.audio.ingest_port
            );
            assert!(
                now.overlay.bg_opacity <= 100,
                "bg_opacity was {}",
                now.overlay.bg_opacity
            );
            assert!(
                now.overlay.border_radius <= 512,
                "border_radius was {}",
                now.overlay.border_radius
            );
            assert!(
                now.overlay.max_lines <= 4,
                "max_lines was {}",
                now.overlay.max_lines
            );
            assert!(
                now.filter.silence_rms <= 1.0,
                "silence_rms was {}",
                now.filter.silence_rms
            );
        }

        // --- nothing that was refused ever reached the state ---------------
        let after = state.config.read().clone();
        assert_eq!(after.audio.sample_rate, before.audio.sample_rate);
        assert_eq!(after.audio.mode, before.audio.mode);
        assert_eq!(after.recording_dir, before.recording_dir);
        assert_eq!(after.llm.model, before.llm.model);
        assert_eq!(after.llm.hotwords, before.llm.hotwords);
        assert_eq!(after.retention_days, before.retention_days);
        assert_eq!(after.llm.endpoint, before.llm.endpoint);
        drop(after);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-02 end to end: changing only the endpoint must not let the saved key
    /// ride along to the new host. The server drops it and says so.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changing_the_endpoint_alone_drops_the_saved_key() {
        const OFFICIAL: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference";
        const WORKSPACE: &str = "wss://ws-demo123.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference";

        let (base, state, dir) = start_server("trustdomain").await;
        {
            let mut cfg = state.config.write();
            cfg.llm.provider = "bailian-fun-asr".into();
            cfg.llm.endpoint = Some(OFFICIAL.into());
            cfg.llm.api_key = "sk-saved-for-the-official-endpoint".into();
        }
        let admin = state.tokens.admin.clone();

        // A panel save that only moves the endpoint, with api_key empty ("keep
        // the saved key") 鈥?the exact exfiltration chain from the audit.
        let patch = serde_json::json!({
            "llm": { "endpoint": WORKSPACE, "api_key": "" }
        });
        let response = http(
            &base,
            "POST",
            "/api/config",
            Some(&admin),
            None,
            Some(&patch.to_string()),
        );
        assert_eq!(
            response.status, 409,
            "the trust-domain change must be refused, not silently applied"
        );
        let body = response.json();
        assert!(
            body.get("api_key_dropped").is_some(),
            "the panel needs to know the key was dropped: {body}"
        );
        assert!(
            state.config.read().llm.api_key.is_empty(),
            "the saved key must not survive a move to a new endpoint"
        );
        assert!(
            !dir.join("config.toml").exists(),
            "a refused save must not write the config"
        );

        // Re-entering the key for the new endpoint is allowed and is recorded as
        // the new trust domain.
        let retry = serde_json::json!({
            "llm": { "endpoint": WORKSPACE, "api_key": "sk-typed-for-the-new-endpoint" }
        });
        let response = http(
            &base,
            "POST",
            "/api/config",
            Some(&admin),
            None,
            Some(&retry.to_string()),
        );
        assert_eq!(
            response.status, 200,
            "an explicitly supplied key for the new endpoint is accepted: {}",
            response.body
        );
        assert!(
            dir.join("config.toml").exists(),
            "the save must now be persisted"
        );
        assert_eq!(
            state.config.read().llm.api_key,
            "sk-typed-for-the-new-endpoint"
        );

        // 鈥nd now that the key belongs to WORKSPACE, moving back to the official
        // endpoint must drop it again rather than silently reusing it.
        let back = serde_json::json!({
            "llm": { "endpoint": OFFICIAL, "api_key": "" }
        });
        let response = http(
            &base,
            "POST",
            "/api/config",
            Some(&admin),
            None,
            Some(&back.to_string()),
        );
        assert_eq!(response.status, 409, "moving back must drop the key too");
        assert!(state.config.read().llm.api_key.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-02: an endpoint outside the provider's allow-list must never receive a
    /// saved key 鈥?the connection is refused, so the key cannot even leave.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_non_allowlisted_endpoint_is_refused_for_a_cloud_provider() {
        let (base, state, dir) = start_server("allowlist").await;
        {
            let mut cfg = state.config.write();
            cfg.llm.provider = "qwen-realtime".into();
            cfg.llm.endpoint = Some("wss://dashscope.aliyuncs.com/api-ws/v1/realtime".into());
            cfg.llm.api_key = "sk-real-cloud-key".into();
        }
        let admin = state.tokens.admin.clone();
        let patch = serde_json::json!({
            "llm": {
                "endpoint": "wss://collect.example.com/steal",
                "api_key": "sk-typed-for-the-unknown-host"
            }
        });
        let response = http(
            &base,
            "POST",
            "/api/config",
            Some(&admin),
            None,
            Some(&patch.to_string()),
        );
        assert_eq!(
            response.status, 400,
            "the pipeline/connection-test policy must reject an unknown host: {}",
            response.body
        );
        assert!(
            !state.config.read().llm.api_key.contains("unknown-host"),
            "the key must not be stored for a host we will refuse to reach anyway"
        );
        assert!(
            !state
                .config
                .read()
                .llm
                .endpoint
                .as_deref()
                .unwrap_or("")
                .contains("collect.example.com"),
            "the refused endpoint must not be persisted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Any legal port/size values are clamped rather than rejected, so a panel
    /// save with a silly-but-harmless number still succeeds.
    #[test]
    fn presentation_values_are_clamped_rather_than_rejected() {
        let mut cfg = Config::default();
        cfg.overlay.bg_opacity = 5_000;
        cfg.overlay.border_radius = 99_999;
        cfg.overlay.font_size = 100_000;
        cfg.overlay.max_lines = 99;
        cfg.audio.ingest_port = 80;
        cfg.normalise(std::path::Path::new("target/test-config.toml"))
            .expect("clamping must make this config valid");
        assert_eq!(cfg.overlay.bg_opacity, 100);
        assert_eq!(cfg.overlay.border_radius, 512);
        assert_eq!(cfg.overlay.font_size, 400);
        assert_eq!(
            cfg.overlay.max_lines, 4,
            "the overlay caps live captions at 4 lines"
        );
        assert_eq!(cfg.audio.ingest_port, 8788);
        // A legal zero must survive clamping: 0 = fully transparent / auto size.
        let mut zero = Config::default();
        zero.overlay.bg_opacity = 0;
        zero.overlay.border_radius = 0;
        zero.overlay.bg_width = 0;
        zero.normalise(std::path::Path::new("target/test-config.toml"))
            .expect("zeros are legal");
        assert_eq!(zero.overlay.bg_opacity, 0);
        assert_eq!(zero.overlay.border_radius, 0);
        assert_eq!(zero.overlay.bg_width, 0);
    }

    /// The asset routes are token-free (the page shell must load without one), so
    /// the path itself is the boundary. A traversal here would read
    /// `config.toml` 鈥?which contains the API key.
    #[test]
    fn asset_paths_cannot_escape_their_directory() {
        for good in [
            "admin/index.html",
            "admin/app.js",
            "overlay/style.css",
            "dist/config.toml",
            "a/b/c.js",
        ] {
            assert_eq!(safe_asset_path(good), Some(good), "{good} must be allowed");
        }
        for bad in [
            "../config.toml",
            "admin/../../config.toml",
            "..\\config.toml",
            "admin/..\\..\\config.toml",
            "/config.toml",
            "\\config.toml",
            "C:/Windows/win.ini",
            "C:config.toml",
            "",
            "admin/\0/app.js",
            "./admin/../config.toml",
        ] {
            assert_eq!(safe_asset_path(bad), None, "{bad:?} must be refused");
        }
    }

    /// The token injection must never let a token break out of the JS string
    /// literal, must substitute only the quoted value (never the identifier), and
    /// must leave a page without the placeholder untouched.
    #[test]
    fn token_injection_is_literal_safe() {
        let page = b"<script>window.__SLT_TOKEN__ = \"__SLT_TOKEN__\";</script>".to_vec();
        let injected = inject_token(page.clone(), "abc123");
        assert_eq!(
            String::from_utf8(injected).unwrap(),
            "<script>window.__SLT_TOKEN__ = \"abc123\";</script>"
        );
        // Quotes, backslashes and newlines are stripped, not escaped, so no
        // token value can terminate the literal and inject script. The identifier
        // must survive untouched 鈥?only the quoted value is substituted.
        let nasty = inject_token(page.clone(), "a\";alert(1)//\n");
        let text = String::from_utf8(nasty).unwrap();
        assert!(!text.contains("alert(1)"), "injection survived: {text}");
        assert!(
            !text.contains('\n'),
            "no raw newline may be injected: {text}"
        );
        assert!(
            text.contains("window.__SLT_TOKEN__ = \""),
            "the identifier must not be substituted away: {text}"
        );
        assert!(
            text.contains("= \"aalert1\";"),
            "the sanitised token must land in the literal: {text}"
        );
        // A page with no placeholder is returned unchanged.
        let plain = b"<html></html>".to_vec();
        assert_eq!(inject_token(plain.clone(), "x"), plain);
        // A real 64-char hex token round-trips into the literal exactly.
        let real = "a".repeat(64);
        let page2 = b"<script>window.__SLT_TOKEN__ = \"__SLT_TOKEN__\";</script>".to_vec();
        let out = String::from_utf8(inject_token(page2, &real)).unwrap();
        assert_eq!(
            out,
            format!("<script>window.__SLT_TOKEN__ = \"{real}\";</script>")
        );
    }
}
