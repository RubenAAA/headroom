//! GitHub Copilot OAuth device-code login for the CLI (Rust port of the CLI
//! slice of `headroom/copilot_auth.py`: device flow start/poll, token
//! save/read, fingerprint). The proxy-side auth resolution (keychain, gh CLI
//! discovery, token exchange) is not ported here.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const DEFAULT_GITHUB_HOST: &str = "github.com";
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

type Error = Box<dyn std::error::Error>;

/// Stable non-secret fingerprint for comparing token handoffs.
pub fn token_fingerprint(token: &str) -> String {
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    format!("sha256:{}", &digest[..12])
}

/// Where Headroom stores its Copilot OAuth token.
pub fn auth_path() -> PathBuf {
    headroom_core::paths::copilot_auth_path()
}

/// Normalize a login domain to a bare lowercase hostname (mirrors Python's
/// `_github_oauth_domain`): strip scheme/path, fall back to github.com.
pub fn oauth_domain(domain: &str) -> String {
    let raw = domain.trim();
    if raw.is_empty() {
        return DEFAULT_GITHUB_HOST.to_string();
    }
    let stripped = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw)
        .trim_end_matches('/');
    // FINDING-032: strip userinfo, then a bracketed IPv6 host (with an
    // optional :port), then path — naive `.split(':').next()` turned
    // `[::1]:8443` into `[`.
    let after_at = stripped.rsplit('@').next().unwrap_or("");
    let no_path = after_at.split('/').next().unwrap_or("");
    let host = if let Some(rest) = no_path.strip_prefix('[') {
        // Bracketed IPv6: host is everything up to `]`; `:port` follows.
        match rest.find(']') {
            Some(end) => &rest[..end],
            None => rest,
        }
    } else if no_path.matches(':').count() > 1 {
        // Bare IPv6 without brackets — no port form is valid here, so
        // splitting would corrupt it; keep whole.
        no_path
    } else {
        // Host or host:port — drop the port.
        no_path.split(':').next().unwrap_or("")
    };
    let host = host.to_lowercase();
    if host.is_empty() {
        DEFAULT_GITHUB_HOST.to_string()
    } else {
        host
    }
}

/// Fields the login flow needs from GitHub's device-code response.
pub struct DeviceAuth {
    pub verification_uri: String,
    pub user_code: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn post_json(url: &str, body: &Value) -> Result<Value, Error> {
    // FINDING-032: surface the server's error message — without
    // error_for_status the caller saw only a JSON-decode failure on a
    // 4xx/5xx HTML or error-JSON body.
    let resp = headroom_proxy::ssl_context::blocking_client_builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .post(url)
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(body)
        .send()?
        .error_for_status()?;
    Ok(resp.json()?)
}

/// Start the GitHub Copilot OAuth device-code flow.
pub fn start_device_authorization(domain: &str) -> Result<DeviceAuth, Error> {
    let host = oauth_domain(domain);
    let payload = post_json(
        &format!("https://{host}/login/device/code"),
        &json!({"client_id": CLIENT_ID, "scope": "read:user"}),
    )?;
    let field = |key: &str| -> String {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let auth = DeviceAuth {
        verification_uri: field("verification_uri"),
        user_code: field("user_code"),
        device_code: field("device_code"),
        interval: payload
            .get("interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(5),
        expires_in: payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(900),
    };
    if auth.verification_uri.is_empty() || auth.user_code.is_empty() || auth.device_code.is_empty()
    {
        return Err("GitHub device login returned an incomplete response.".into());
    }
    Ok(auth)
}

/// What one poll response tells us to do next.
#[derive(Debug, PartialEq)]
pub enum PollStep {
    Token(String),
    /// Sleep and retry; the value is the increment to add to the interval.
    Retry {
        interval_bump: u64,
    },
    Failed(String),
}

/// Interpret one access-token poll payload (pure, for tests).
pub fn interpret_poll_payload(payload: &Value) -> PollStep {
    if let Some(token) = payload.get("access_token").and_then(|v| v.as_str()) {
        let token = token.trim();
        if !token.is_empty() {
            return PollStep::Token(token.to_string());
        }
    }
    let error = payload
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match error {
        "" | "authorization_pending" => PollStep::Retry { interval_bump: 0 },
        "slow_down" => PollStep::Retry { interval_bump: 5 },
        "expired_token" => PollStep::Failed("GitHub device authorization expired.".to_string()),
        _ => {
            let description = payload
                .get("error_description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(error);
            PollStep::Failed(format!("GitHub device authorization failed: {description}"))
        }
    }
}

/// Poll GitHub until the device-code flow returns an access token.
pub fn poll_device_authorization(
    device_code: &str,
    domain: &str,
    interval: u64,
    expires_in: u64,
) -> Result<String, Error> {
    let host = oauth_domain(domain);
    let url = format!("https://{host}/login/oauth/access_token");
    let deadline = Instant::now() + Duration::from_secs(expires_in.max(1));
    let mut poll_interval = interval.max(1);
    while Instant::now() < deadline {
        let payload = post_json(
            &url,
            &json!({
                "client_id": CLIENT_ID,
                "device_code": device_code,
                "grant_type": DEVICE_CODE_GRANT_TYPE,
            }),
        )?;
        match interpret_poll_payload(&payload) {
            PollStep::Token(token) => return Ok(token),
            PollStep::Retry { interval_bump } => {
                poll_interval += interval_bump;
                std::thread::sleep(Duration::from_secs(poll_interval));
            }
            PollStep::Failed(message) => return Err(message.into()),
        }
    }
    Err("GitHub device authorization expired.".into())
}

/// Persist the OAuth token; returns the path written. File is chmod 0600.
pub fn save_oauth_token(token: &str, domain: &str) -> Result<PathBuf, Error> {
    let token = token.trim();
    if token.is_empty() {
        return Err("Copilot OAuth token must not be empty.".into());
    }
    let path = auth_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = json!({
        "created_at": created_at,
        "domain": oauth_domain(domain),
        "provider": "github-copilot",
        "refresh": token,
        "type": "oauth",
    });
    // FINDING-032 (TOCTOU): create with 0600 atomically — write-then-chmod
    // leaves the token world-readable under a standard 022 umask until the
    // chmod lands. OpenOptions with `.create_new()` also avoids clobbering
    // an existing token file's permissions on re-save.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        opts.mode(0o600);
        opts.open(&path)?
            .write_all(format!("{}\n", serde_json::to_string_pretty(&body)?).as_bytes())?;
        // Re-saving must tighten, not just create: an existing file keeps
        // its old mode through open().
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&body)?))?;
    }
    Ok(path)
}

/// Read the saved OAuth token, if one is available and well-formed.
pub fn read_oauth_token() -> Option<String> {
    let text = std::fs::read_to_string(auth_path()).ok()?;
    let payload: Value = serde_json::from_str(&text).ok()?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("oauth") {
        return None;
    }
    let token = payload.get("refresh")?.as_str()?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_short() {
        let fp = token_fingerprint("gho_example");
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), "sha256:".len() + 12);
        assert_eq!(fp, token_fingerprint("gho_example"));
        assert_ne!(fp, token_fingerprint("gho_other"));
    }

    #[test]
    fn oauth_domain_normalizes() {
        assert_eq!(oauth_domain(""), "github.com");
        assert_eq!(oauth_domain("GitHub.COM"), "github.com");
        assert_eq!(oauth_domain("https://ghe.example.com/"), "ghe.example.com");
        assert_eq!(
            oauth_domain("http://ghe.example.com/enterprises/acme"),
            "ghe.example.com"
        );
        assert_eq!(oauth_domain("ghe.example.com:8443"), "ghe.example.com");
        // FINDING-032: bracketed + bare IPv6 must survive intact.
        assert_eq!(oauth_domain("[::1]:8443"), "::1");
        assert_eq!(oauth_domain("http://[::1]:8443/"), "::1");
        assert_eq!(oauth_domain("::1"), "::1");
    }

    #[test]
    fn poll_payload_interpretation() {
        assert_eq!(
            interpret_poll_payload(&json!({"access_token": " tok "})),
            PollStep::Token("tok".to_string())
        );
        assert_eq!(
            interpret_poll_payload(&json!({"error": "authorization_pending"})),
            PollStep::Retry { interval_bump: 0 }
        );
        assert_eq!(
            interpret_poll_payload(&json!({"error": "slow_down"})),
            PollStep::Retry { interval_bump: 5 }
        );
        assert_eq!(
            interpret_poll_payload(&json!({"error": "expired_token"})),
            PollStep::Failed("GitHub device authorization expired.".to_string())
        );
        assert_eq!(
            interpret_poll_payload(&json!({
                "error": "access_denied",
                "error_description": "The user denied the request."
            })),
            PollStep::Failed(
                "GitHub device authorization failed: The user denied the request.".to_string()
            )
        );
        // Empty payload → keep polling.
        assert_eq!(
            interpret_poll_payload(&json!({})),
            PollStep::Retry { interval_bump: 0 }
        );
    }

    #[test]
    fn save_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        // Env var mutation is process-wide; this is the only test using it.
        std::env::set_var(headroom_core::paths::HEADROOM_COPILOT_AUTH_FILE_ENV, &file);
        let path = save_oauth_token("gho_secret", "GitHub.com").unwrap();
        assert_eq!(path, file);
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(payload["type"], "oauth");
        assert_eq!(payload["provider"], "github-copilot");
        assert_eq!(payload["domain"], "github.com");
        assert_eq!(read_oauth_token().as_deref(), Some("gho_secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Wrong type → None.
        std::fs::write(&file, r#"{"type":"pat","refresh":"x"}"#).unwrap();
        assert_eq!(read_oauth_token(), None);
        std::env::remove_var(headroom_core::paths::HEADROOM_COPILOT_AUTH_FILE_ENV);
    }

    #[test]
    fn save_rejects_empty_token() {
        assert!(save_oauth_token("   ", "github.com").is_err());
    }
}
