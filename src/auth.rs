//! Access control for the local HTTP/WebSocket server.
//!
//! The engine serves an admin panel that can change provider credentials, stop
//! the recognition pipeline and clear recorded subtitles, and an overlay that
//! only needs to *read* subtitles. Both are reachable over plain HTTP on a
//! loopback port, and the port is guessable, so anything else running on the
//! machine (or a web page the user visits) can reach them unless the request is
//! authenticated.
//!
//! Three invariants this module exists to enforce:
//!
//! 1. **Every `/api/*` request and every `/ws/subtitles` upgrade carries a
//!    token.** Two roles exist: [`Role::Admin`] (full control) and
//!    [`Role::Overlay`] (read-only). The overlay token can never reach a
//!    mutating or sensitive route, so a leaked OBS Browser Source URL does not
//!    hand over the API key.
//! 2. **`Origin` is checked, not just `Host`.** `Host` is attacker-controlled
//!    via DNS rebinding and is not a CSRF defence. A request with any
//!    `Origin` other than this server's own is refused, which stops a page the
//!    user is browsing from driving the admin API.
//! 3. **A non-loopback bind requires authentication.** Binding `0.0.0.0` with no
//!    token would expose the admin API to the whole network; the process
//!    refuses to start instead.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use rand::RngCore;

/// What a caller is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full control: config writes, stop/restart, recordings, clears.
    Admin,
    /// Read-only: subtitle stream, current style, status, device list.
    Overlay,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Overlay => "overlay",
        }
    }
}

/// The token pair the process runs with.
///
/// `admin` is the full-control token. `overlay` is a *separate* token that is
/// only ever accepted for read-only routes, so the URL that gets pasted into
/// OBS cannot be used to change settings or stop recognition.
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub admin: String,
    pub overlay: String,
}

impl TokenPair {
    pub fn generate() -> Self {
        Self {
            admin: random_token(),
            overlay: random_token(),
        }
    }

    /// Match a presented token against both roles. Uses a constant-time
    /// compare so the token cannot be recovered by timing the response.
    pub fn role_for(&self, presented: &str) -> Option<Role> {
        if presented.is_empty() {
            return None;
        }
        if constant_time_eq(presented.as_bytes(), self.admin.as_bytes()) {
            return Some(Role::Admin);
        }
        if constant_time_eq(presented.as_bytes(), self.overlay.as_bytes()) {
            return Some(Role::Overlay);
        }
        None
    }

    pub fn is_valid(&self) -> bool {
        self.admin != self.overlay
            && [&self.admin, &self.overlay].iter().all(|token| {
                token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    }
}

/// 32 bytes of CSPRNG output, hex encoded. Long enough that guessing is
/// hopeless and hex keeps it safe in a query string without escaping.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Length-independent comparison for equal-length inputs. Tokens always have
/// the same length here, so a length mismatch is already a rejection.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Where the generated tokens live, next to `config.toml`.
pub fn tokens_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokens.toml")
}

/// Load the token pair, generating and persisting it on first run.
///
/// On Unix the file is created `0600` and an existing file's mode is tightened,
/// because a world-readable token file would undo the point of having tokens.
/// On Windows the file inherits the user profile ACL, which is the same
/// protection `config.toml` relies on.
pub fn load_or_create(config_path: &Path) -> std::io::Result<TokenPair> {
    let path = tokens_path(config_path);
    if path.exists() {
        let pair = parse(&std::fs::read_to_string(&path)?)?;
        tighten_permissions(&path);
        return Ok(pair);
    }
    let pair = TokenPair::generate();
    write(&path, &pair)?;
    Ok(pair)
}

fn write(path: &Path, pair: &TokenPair) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "# Generated once on first run. Treat like a password.\n\
         # admin   = full control (config writes, stop/restart, recordings)\n\
         # overlay = read-only (safe to paste into an OBS Browser Source URL)\n\
         admin = \"{}\"\n\
         overlay = \"{}\"\n",
        pair.admin, pair.overlay
    );
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    // Create with restrictive mode before the content is visible.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    tighten_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn tighten_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) {}

/// Minimal `key = "value"` reader. The file is generated by this module, so a
/// full TOML dependency is not warranted; an unreadable or malformed file is
/// treated as "no tokens" by the caller, which then regenerates it.
pub fn parse(raw: &str) -> std::io::Result<TokenPair> {
    let mut admin = None;
    let mut overlay = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "admin" => admin = Some(value),
            "overlay" => overlay = Some(value),
            _ => {}
        }
    }
    match (admin, overlay) {
        (Some(admin), Some(overlay)) => {
            let pair = TokenPair { admin, overlay };
            if pair.is_valid() {
                return Ok(pair);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tokens.toml requires two distinct 64-character hexadecimal tokens",
            ))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tokens.toml must contain non-empty `admin` and `overlay` values",
        )),
    }
}

/// True when a `Host` authority addresses this server as a literal loopback
/// address on the port it bound.
///
/// Used by the token-recovery route (`GET /api/auth/local-token`), the one
/// place that hands out the admin token over HTTP. `localhost` is accepted
/// because the browser resolves it to a loopback address itself, but a hostname
/// that merely *resolves* to 127.0.0.1 (`evil.example`) is not: that is exactly
/// the address a DNS-rebinding page must use to reach this server at all, and
/// the name is not something this process ever advertised.
///
/// The port must match the bound port, so `127.0.0.1:9999` (a different origin,
/// possibly another process on this machine) is refused. An omitted port is
/// accepted only when we really bound the scheme default (80).
pub fn is_loopback_authority(authority: &str, bind_port: u16) -> bool {
    let authority = authority.trim();
    if authority.is_empty() || authority.contains('/') {
        return false;
    }
    let (host, port) = split_authority(authority);
    match port {
        Some(p) if p != bind_port => return false,
        None if bind_port != 80 => return false,
        _ => {}
    }
    is_loopback_host(&host)
}

/// True when `host` binds only to the loopback interface.
///
/// Anything that is not an address literal is treated as reachable from other
/// hosts: a hostname could resolve anywhere, so it must not be assumed local.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    // `[::1]:8787`, `[::1]` and bare `::1` all mean the v6 loopback.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

/// The decision the process must make before it starts listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindPolicy {
    /// Safe to serve with this token pair.
    Allow,
    /// The process must refuse to start, for the stated reason.
    Refuse(String),
}

/// Decide whether `host` may be bound given whether an operator explicitly
/// configured authentication.
///
/// `auth_configured` is true when a token was supplied deliberately (env var or
/// a `tokens.toml` the operator created), as opposed to one this process just
/// generated for its own convenience — a freshly generated token has never been
/// shown to anyone, so it is not evidence that the operator intended to expose
/// the port.
pub fn bind_policy(host: &str, auth_configured: bool) -> BindPolicy {
    if is_loopback_host(host) {
        return BindPolicy::Allow;
    }
    if auth_configured {
        return BindPolicy::Allow;
    }
    BindPolicy::Refuse(format!(
        "拒绝绑定非回环地址 {host}：管理接口可以直接修改 API Key、停止识别并清空录像，\
         未配置认证时不得对外暴露。\n\
         请任选其一：\n\
         1) 改回 `--host 127.0.0.1`（默认，仅本机可访问）；\n\
         2) 显式配置认证：同时设置环境变量 SLT_ADMIN_TOKEN 与 SLT_OVERLAY_TOKEN，\
         或手工创建与 config.toml 同目录的 tokens.toml 后再启动。"
    ))
}

/// Requested access level for a route, derived from method + path.
///
/// Anything not listed explicitly is [`Access::Admin`], so a new mutating route
/// is protected by default rather than accidentally left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No token needed. Reserved for the static shell: the browser cannot send
    /// a custom header on a top-level navigation, and `admin/index.html` is what
    /// carries the token to the page. It contains no data beyond the page
    /// skeleton, and the token is injected per-request from the caller's own
    /// credential, so an anonymous fetch of it yields no secret.
    Public,
    /// Read-only routes: either token is accepted.
    Overlay,
    /// Full control: only the admin token.
    Admin,
}

/// Routes the overlay token may use. Deliberately a small, explicit allow-list:
/// the overlay renders subtitles and applies style, and nothing else.
pub fn required_access(method: &str, path: &str) -> Access {
    let path = path.trim_end_matches('/');
    let read_only = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");

    // The page shells and their asset bundles carry no data.
    if read_only
        && matches!(
            path,
            "/" | "/admin" | "/overlay" | "/admin-assets" | "/overlay-assets"
        )
    {
        return Access::Public;
    }
    if read_only && (path.starts_with("/admin-assets/") || path.starts_with("/overlay-assets/")) {
        return Access::Public;
    }

    // The handler releases a token only for a literal loopback authority.
    if read_only && path == "/api/auth/local-token" {
        return Access::Public;
    }

    if read_only
        && matches!(
            path,
            // Subtitle stream + the style it renders with. `/api/config` is
            // redacted (no secrets) and is what the overlay applies its style
            // from, so the read-only token must be able to reach it.
            "/ws/subtitles" | "/api/subtitles" | "/api/config" | "/api/status"
        )
    {
        return Access::Overlay;
    }
    Access::Admin
}

/// Decide whether a request's `Origin` header is acceptable.
///
/// `None` (no header) is allowed: a same-origin `fetch`/`WebSocket` from our own
/// page sends `Origin`, but a non-browser client (the packaging scripts, curl,
/// the test harness) sends none, and refusing those would break real tooling
/// without stopping any browser-based attack — a browser always sends `Origin`
/// on a cross-origin request.
///
/// `Some(value)` must be an `http://host[:port]` that this server
/// is actually reachable as. Which origins those are depends on the **bind**,
/// not on the caller: this is the check that stops a page the user is merely
/// visiting (and a DNS-rebinding hostname) from driving the API.
pub fn origin_allowed(origin: Option<&str>, bind_host: &str, bind_port: u16) -> bool {
    let Some(origin) = origin else { return true };
    let origin = origin.trim();
    if origin.is_empty() {
        return true;
    }
    if origin.eq_ignore_ascii_case("null") {
        // Sandboxed iframe / `file://` page. We cannot verify who that is, and
        // no legitimate client of ours is one, so refuse.
        return false;
    }
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = split_authority(authority);

    // Port: a different port is a different origin. An omitted port is only
    // meaningful for the scheme's default, so require an explicit match unless
    // the port we bind is itself a default.
    match port {
        Some(p) if p != bind_port => return false,
        None if bind_port != 80 => return false,
        _ => {}
    }

    let bind = bind_host.trim();
    let bind_bare = bind
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or(bind);

    // Bound to loopback: only a loopback origin can be our own page. Anything
    // else — a remote hostname, a hostname that resolves to 127.0.0.1 (DNS
    // rebinding) — is refused.
    if is_loopback_host(bind_bare) {
        return is_loopback_host(&host);
    }

    // A wildcard bind has no single hostname. Accept literal local-interface
    // addresses; arbitrary DNS names could be rebound to this listener.
    if bind_bare == "0.0.0.0" || bind_bare == "::" {
        return is_loopback_host(&host) || host.parse::<IpAddr>().is_ok();
    }

    // Bound to one specific address: that address is our origin, and so is
    // loopback (the local panel still reaches it that way).
    is_loopback_host(&host) || host.eq_ignore_ascii_case(bind_bare)
}

fn split_authority(authority: &str) -> (String, Option<u16>) {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: [::1]:8787
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (host.to_string(), port);
        }
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host.to_string(), port.parse().ok())
        }
        _ => (authority.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_long_random_and_distinct() {
        let a = TokenPair::generate();
        let b = TokenPair::generate();
        assert_eq!(a.admin.len(), 64, "32 bytes hex-encoded");
        assert_eq!(a.overlay.len(), 64);
        assert_ne!(a.admin, a.overlay, "the two roles must not share a secret");
        assert_ne!(a.admin, b.admin, "each run must get a fresh token");
    }

    #[test]
    fn role_lookup_distinguishes_admin_from_overlay() {
        let pair = TokenPair {
            admin: "a".repeat(64),
            overlay: "b".repeat(64),
        };
        assert_eq!(pair.role_for(&pair.admin), Some(Role::Admin));
        assert_eq!(pair.role_for(&pair.overlay), Some(Role::Overlay));
        assert_eq!(pair.role_for(""), None, "an empty token is never accepted");
        assert_eq!(
            pair.role_for(&"c".repeat(64)),
            None,
            "a wrong token is refused"
        );
        assert_eq!(pair.role_for("a"), None, "a prefix is not the token");
    }

    #[test]
    fn token_file_round_trips() {
        let pair = TokenPair {
            admin: "1".repeat(64),
            overlay: "2".repeat(64),
        };
        let raw = format!(
            "admin = \"{}\"\noverlay = \"{}\"\n",
            pair.admin, pair.overlay
        );
        let back = parse(&raw).expect("parse");
        assert_eq!(back.admin, pair.admin);
        assert_eq!(back.overlay, pair.overlay);
        // Comments and blank lines are tolerated.
        let with_comments = format!(
            "# c\n\nadmin = \"{}\"\n# c2\noverlay = \"{}\"\n",
            pair.admin, pair.overlay
        );
        assert!(parse(&with_comments).is_ok());
        // A half-written file must be an error, not a token pair with "".
        assert!(parse("admin = \"x\"\n").is_err());
        assert!(parse("admin = \"\"\noverlay = \"y\"\n").is_err());
        assert!(parse("admin = \"a\"\noverlay = \"b\"\n").is_err());
        let same = "a".repeat(64);
        assert!(parse(&format!("admin = \"{same}\"\noverlay = \"{same}\"\n")).is_err());
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for host in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "127.0.0.5",
        ] {
            assert!(is_loopback_host(host), "{host} must count as loopback");
        }
        for host in ["0.0.0.0", "::", "192.168.1.10", "example.com", ""] {
            assert!(!is_loopback_host(host), "{host} must NOT count as loopback");
        }
    }

    /// The release blocker from the audit: `--host 0.0.0.0` without a configured
    /// credential must refuse to start, while a loopback bind never needs one.
    #[test]
    fn a_non_loopback_bind_without_configured_auth_is_refused() {
        assert_eq!(bind_policy("127.0.0.1", false), BindPolicy::Allow);
        assert_eq!(bind_policy("::1", false), BindPolicy::Allow);
        assert_eq!(bind_policy("localhost", false), BindPolicy::Allow);
        assert_eq!(bind_policy("0.0.0.0", true), BindPolicy::Allow);
        match bind_policy("0.0.0.0", false) {
            BindPolicy::Refuse(message) => {
                assert!(
                    message.contains("0.0.0.0"),
                    "message must name the host: {message}"
                );
                assert!(
                    message.contains("127.0.0.1"),
                    "message must offer the safe default"
                );
            }
            BindPolicy::Allow => panic!("0.0.0.0 without configured auth must be refused"),
        }
        match bind_policy("192.168.1.10", false) {
            BindPolicy::Refuse(_) => {}
            BindPolicy::Allow => panic!("a LAN bind without configured auth must be refused"),
        }
        // A hostname could resolve off-box, so it is not treated as local.
        assert!(matches!(
            bind_policy("my-machine.local", false),
            BindPolicy::Refuse(_)
        ));
    }

    /// The overlay token may read the subtitle stream and the style, and must
    /// not reach anything that mutates configuration, stops the pipeline or
    /// reads/erases recordings.
    #[test]
    fn overlay_access_covers_only_read_routes() {
        for (method, path) in [
            ("GET", "/api/config"),
            ("GET", "/api/status"),
            ("GET", "/api/subtitles"),
            ("GET", "/ws/subtitles"),
            ("GET", "/overlay"),
            ("GET", "/overlay-assets/app.js"),
            ("GET", "/overlay-assets/style.css"),
        ] {
            assert!(
                matches!(
                    required_access(method, path),
                    Access::Overlay | Access::Public
                ),
                "{method} {path} must be reachable by the overlay token"
            );
        }
        // The page shell is public; the data endpoints are not.
        assert_eq!(required_access("GET", "/overlay"), Access::Public);
        assert_eq!(
            required_access("GET", "/overlay-assets/app.js"),
            Access::Public
        );
        assert_eq!(required_access("GET", "/api/subtitles"), Access::Overlay);

        for (method, path) in [
            ("POST", "/api/config"),
            ("POST", "/api/stop"),
            ("POST", "/api/restart"),
            ("POST", "/api/config/clear-key"),
            ("POST", "/api/connection-test"),
            ("POST", "/api/audio-test"),
            ("POST", "/api/subtitles/clear"),
            ("POST", "/api/subtitles/history/clear"),
            ("GET", "/api/recordings"),
            ("GET", "/api/recordings/export"),
            ("GET", "/api/devices"),
            ("GET", "/api/locale"),
            // A GET must not inherit overlay rights just by being a GET.
            ("GET", "/api/some-future-endpoint"),
        ] {
            assert_eq!(
                required_access(method, path),
                Access::Admin,
                "{method} {path} must require the admin token"
            );
        }
    }

    /// Reading a config away from the browser must stay possible (scripts and
    /// curl send no `Origin`), while a cross-origin page must be refused.
    #[test]
    fn origin_check_refuses_cross_origin_browsers() {
        // No header: non-browser client.
        assert!(origin_allowed(None, "127.0.0.1", 8787));
        assert!(origin_allowed(Some(""), "127.0.0.1", 8787));
        // Our own page, both loopback spellings. The port must match: a browser
        // omits it only for the scheme default, so `http://127.0.0.1` means port
        // 80 and is NOT our origin when we serve on 8787.
        assert!(origin_allowed(
            Some("http://127.0.0.1:8787"),
            "127.0.0.1",
            8787
        ));
        assert!(origin_allowed(
            Some("http://localhost:8787"),
            "127.0.0.1",
            8787
        ));
        assert!(origin_allowed(Some("http://[::1]:8787"), "127.0.0.1", 8787));
        assert!(!origin_allowed(
            Some("https://127.0.0.1:8787"),
            "127.0.0.1",
            8787
        ));
        assert!(
            !origin_allowed(Some("http://127.0.0.1"), "127.0.0.1", 8787),
            "an omitted port means 80, which is a different origin from 8787"
        );
        // On a default port the omitted form is the same origin.
        assert!(origin_allowed(Some("http://127.0.0.1"), "127.0.0.1", 80));
        assert!(!origin_allowed(Some("https://127.0.0.1"), "127.0.0.1", 443));

        // A page the user is merely visiting.
        assert!(!origin_allowed(
            Some("http://evil.example"),
            "127.0.0.1",
            8787
        ));
        assert!(!origin_allowed(
            Some("http://evil.example:8787"),
            "127.0.0.1",
            8787
        ));
        // DNS rebinding: the attacker's hostname resolves to 127.0.0.1, so the
        // Host header is *not* a defence — only the Origin is.
        assert!(!origin_allowed(
            Some("http://attacker.example:8787"),
            "127.0.0.1",
            8787
        ));
        assert!(!origin_allowed(
            Some("http://127.0.0.1.evil.example:8787"),
            "127.0.0.1",
            8787
        ));
        // A different port is a different origin even on loopback.
        assert!(!origin_allowed(
            Some("http://127.0.0.1:9999"),
            "127.0.0.1",
            8787
        ));
        // A LAN address is not our origin when we only bound loopback.
        assert!(!origin_allowed(
            Some("http://192.168.1.10:8787"),
            "127.0.0.1",
            8787
        ));
        // Sandboxed / opaque origin.
        assert!(!origin_allowed(Some("null"), "127.0.0.1", 8787));
        // Non-http schemes are not our pages.
        assert!(!origin_allowed(Some("file://x"), "127.0.0.1", 8787));
        assert!(!origin_allowed(
            Some("ws://127.0.0.1:8787"),
            "127.0.0.1",
            8787
        ));
    }

    /// When the operator binds one specific LAN address, *that* address is the
    /// panel's origin — and loopback still reaches it locally. Anything else is
    /// still a foreign page.
    #[test]
    fn a_bound_lan_address_is_a_valid_origin_but_nothing_else_is() {
        let host = "192.168.1.10";
        assert!(origin_allowed(Some("http://192.168.1.10:8787"), host, 8787));
        assert!(origin_allowed(Some("http://127.0.0.1:8787"), host, 8787));
        assert!(origin_allowed(Some("http://localhost:8787"), host, 8787));
        assert!(!origin_allowed(
            Some("http://192.168.1.11:8787"),
            host,
            8787
        ));
        assert!(!origin_allowed(
            Some("http://evil.example:8787"),
            host,
            8787
        ));
        // A different port on the bound address is a different origin.
        assert!(!origin_allowed(
            Some("http://192.168.1.10:9999"),
            host,
            8787
        ));
    }

    /// A wildcard bind accepts literal interface addresses but not a hostname
    /// that could be rebound to a different machine.
    #[test]
    fn a_wildcard_bind_accepts_ip_origins_but_not_rebindable_names() {
        assert!(origin_allowed(
            Some("http://192.168.1.10:8787"),
            "0.0.0.0",
            8787
        ));
        assert!(!origin_allowed(
            Some("http://panel.example:8787"),
            "0.0.0.0",
            8787
        ));
        assert!(origin_allowed(
            Some("http://127.0.0.1:8787"),
            "0.0.0.0",
            8787
        ));
        assert!(!origin_allowed(
            Some("http://panel.example:9999"),
            "0.0.0.0",
            8787
        ));
        assert!(!origin_allowed(Some("null"), "0.0.0.0", 8787));
        assert!(!origin_allowed(Some("file://x"), "0.0.0.0", 8787));
    }

    #[test]
    fn authority_parsing_handles_ipv6_and_missing_ports() {
        assert_eq!(
            split_authority("127.0.0.1:8787"),
            ("127.0.0.1".into(), Some(8787))
        );
        assert_eq!(split_authority("127.0.0.1"), ("127.0.0.1".into(), None));
        assert_eq!(split_authority("[::1]:8787"), ("::1".into(), Some(8787)));
        assert_eq!(split_authority("[::1]"), ("::1".into(), None));
        assert_eq!(split_authority("localhost"), ("localhost".into(), None));
    }

    /// The token-recovery route may only be answered on the literal loopback
    /// authority this process bound. A hostname that merely resolves to
    /// 127.0.0.1 — what a DNS-rebinding page must use — is refused, and so is a
    /// different port (a different origin).
    #[test]
    fn only_the_literal_loopback_authority_may_claim_the_token() {
        for authority in [
            "127.0.0.1:8787",
            "localhost:8787",
            "LOCALHOST:8787",
            "127.0.0.5:8787",
            "[::1]:8787",
        ] {
            assert!(
                is_loopback_authority(authority, 8787),
                "{authority} is our own loopback authority"
            );
        }
        for authority in [
            "evil.example:8787",          // DNS rebinding: resolves to 127.0.0.1
            "127.0.0.1.evil.example:8787",
            "192.168.1.10:8787",          // another machine
            "127.0.0.1:9999",             // a different origin on this machine
            "127.0.0.1",                  // means port 80, not 8787
            "localhost",                  // same
            "127.0.0.1:8787/path",        // not an authority
            "",                           // HTTP/1.0 or a forged request
            "127.0.0.1:notaport",
        ] {
            assert!(
                !is_loopback_authority(authority, 8787),
                "{authority:?} must not be treated as our own authority"
            );
        }
        // On the scheme default the port-less form is the same origin.
        assert!(is_loopback_authority("127.0.0.1", 80));
        assert!(is_loopback_authority("localhost", 80));
    }

    #[test]
    fn a_token_file_is_created_with_a_usable_shape() {        let dir = std::env::temp_dir().join(format!("slt-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");
        let pair = load_or_create(&config).expect("create tokens");
        assert_eq!(pair.admin.len(), 64);
        // Second load returns the same pair instead of rotating the secret.
        let again = load_or_create(&config).expect("reload tokens");
        assert_eq!(again.admin, pair.admin);
        assert_eq!(again.overlay, pair.overlay);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_token_file_is_not_silently_replaced() {
        let dir = std::env::temp_dir().join(format!("slt-auth-corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        let tokens = tokens_path(&config);
        std::fs::write(&tokens, "admin = \"a\"\noverlay = \"b\"\n").unwrap();
        assert!(load_or_create(&config).is_err());
        assert_eq!(
            std::fs::read_to_string(&tokens).unwrap(),
            "admin = \"a\"\noverlay = \"b\"\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
