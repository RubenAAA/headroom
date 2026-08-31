//! SSL/TLS context builder for the Headroom upstream reqwest client.
//!
//! Respects standard CA-bundle environment variables used by Python
//! (`SSL_CERT_FILE`), requests (`REQUESTS_CA_BUNDLE`), and Node.js /
//! Claude Code (`NODE_EXTRA_CA_CERTS`) so that enterprise / corporate
//! deployments with custom certificate authorities work without extra
//! configuration.
//!
//! Priority order (first match wins):
//! 1. `SSL_CERT_FILE` — replacement semantics (only these CAs are trusted)
//! 2. `REQUESTS_CA_BUNDLE` — replacement semantics
//! 3. `NODE_EXTRA_CA_CERTS` — **additive** semantics (extra roots loaded
//!    on top of the default/system trust store, matching Node.js behavior)
//!
//! Strict-mode toggle (`HEADROOM_TLS_STRICT`):
//! The variable is accepted for configuration parity with the Python proxy,
//! but reqwest's rustls backend has no equivalent to OpenSSL's
//! `VERIFY_X509_STRICT` flag. Normal chain, signature, expiry, and hostname
//! validation therefore remains enabled for every value.
//!
//! Mirrors Python's `headroom.proxy.ssl_context`.

use std::path::{Path, PathBuf};

/// Env var that provides a replacement CA bundle path.
const REPLACEMENT_CA_VARS: &[&str] = &["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"];

/// Env var for additive CA bundle (Node.js behavior).
const ADDITIVE_CA_VAR: &str = "NODE_EXTRA_CA_CERTS";

/// Env var that opts out of OpenSSL's RFC 5280 strict CA-constraint checks.
pub const TLS_STRICT_ENV: &str = "HEADROOM_TLS_STRICT";

/// Values (case-insensitive) that mean "turn strict mode OFF".
const TLS_STRICT_OFF_VALUES: &[&str] = &["0", "false", "no", "off"];

/// Whether the `HEADROOM_TLS_STRICT` env var disables strict mode.
pub fn tls_strict_disabled() -> bool {
    let value = std::env::var(TLS_STRICT_ENV)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    TLS_STRICT_OFF_VALUES.contains(&value.as_str())
}

/// Result of CA bundle detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaBundleResult {
    /// Replacement semantics: only the custom CA bundle is trusted.
    Replacement(PathBuf),
    /// Additive semantics: custom CAs are added to the system trust store.
    Additive(PathBuf),
    /// No custom CA bundle found; use default trust store.
    Default,
}

/// Detect a CA bundle from environment variables.
///
/// Returns the first valid CA bundle path found, with its semantics.
pub fn find_ca_bundle() -> CaBundleResult {
    // Check replacement vars first
    for var in REPLACEMENT_CA_VARS {
        if let Ok(path) = std::env::var(var) {
            let path = PathBuf::from(&path);
            if path.is_file() {
                tracing::info!(
                    event = "ssl_ca_bundle_loaded",
                    env_var = var,
                    path = %path.display(),
                    "CA bundle loaded"
                );
                return CaBundleResult::Replacement(path);
            }
            if !path.as_os_str().is_empty() {
                tracing::warn!(
                    event = "ssl_ca_bundle_missing",
                    env_var = var,
                    path = %path.display(),
                    "CA bundle path not found, skipping"
                );
            }
        }
    }

    // Check additive var
    if let Ok(path) = std::env::var(ADDITIVE_CA_VAR) {
        let path = PathBuf::from(&path);
        if path.is_file() {
            tracing::info!(
                event = "ssl_ca_bundle_loaded",
                env_var = ADDITIVE_CA_VAR,
                path = %path.display(),
                additive = true,
                "CA bundle loaded (additive)"
            );
            return CaBundleResult::Additive(path);
        }
        if !path.as_os_str().is_empty() {
            tracing::warn!(
                event = "ssl_ca_bundle_missing",
                env_var = ADDITIVE_CA_VAR,
                path = %path.display(),
                "CA bundle path not found, skipping"
            );
        }
    }

    CaBundleResult::Default
}

/// Apply the common CA policy to either reqwest builder flavour. Keeping the
/// implementation in one macro matters here: async and blocking reqwest use
/// distinct builder types, but expose the same TLS methods.
macro_rules! configure_tls_builder {
    ($builder:expr) => {{
        let builder = $builder;
        match find_ca_bundle() {
            CaBundleResult::Replacement(path) => match load_certificates_from_file(&path) {
                Ok(certs) => {
                    // SSL_CERT_FILE / REQUESTS_CA_BUNDLE replace the trust
                    // store. Without this call reqwest would silently keep its
                    // built-in WebPKI roots and turn replacement into additive
                    // semantics.
                    let mut configured = builder.tls_built_in_root_certs(false);
                    for cert in certs {
                        configured = configured.add_root_certificate(cert);
                    }
                    configured
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        semantics = "replacement",
                        "failed to load CA certificates; using default TLS"
                    );
                    builder
                }
            },
            CaBundleResult::Additive(path) => match load_certificates_from_file(&path) {
                Ok(certs) => {
                    let mut configured = builder;
                    for cert in certs {
                        configured = configured.add_root_certificate(cert);
                    }
                    configured
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        semantics = "additive",
                        "failed to load CA certificates; using default TLS"
                    );
                    builder
                }
            },
            CaBundleResult::Default => {
                if tls_strict_disabled() {
                    // rustls has no OpenSSL VERIFY_X509_STRICT flag to clear.
                    // It keeps chain, signature, expiry, and hostname checks
                    // enabled, which is the security contract of this toggle.
                    tracing::info!(
                        event = "ssl_x509_strict_disabled",
                        reason = "env_toggle",
                        "TLS strict compatibility toggle accepted; rustls validation remains enabled"
                    );
                }
                builder
            }
        }
    }};
}

/// Configure an asynchronous reqwest builder with Headroom's CA policy.
pub fn configure_client_tls(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    configure_tls_builder!(builder)
}

/// Configure a blocking reqwest builder with Headroom's CA policy.
pub fn configure_blocking_client_tls(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    configure_tls_builder!(builder)
}

/// The canonical constructor for every asynchronous outbound HTTP client.
pub fn client_builder() -> reqwest::ClientBuilder {
    configure_client_tls(reqwest::Client::builder())
}

/// The canonical constructor for every blocking outbound HTTP client.
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    configure_blocking_client_tls(reqwest::blocking::Client::builder())
}

/// Load PEM-encoded CA certificates from a file.
fn load_certificates_from_file(
    path: &Path,
) -> Result<Vec<reqwest::Certificate>, Box<dyn std::error::Error + Send + Sync>> {
    let pem = std::fs::read(path)?;
    let mut certs = Vec::new();

    // Try to parse as a single PEM certificate first
    if let Ok(cert) = reqwest::Certificate::from_pem(&pem) {
        certs.push(cert);
        return Ok(certs);
    }

    // Try parsing as a bundle (multiple PEM certificates concatenated)
    // Split on PEM boundary markers
    let pem_str = String::from_utf8_lossy(&pem);
    let mut current_cert = String::new();
    let mut in_cert = false;

    for line in pem_str.lines() {
        if line.contains("-----BEGIN CERTIFICATE-----") {
            in_cert = true;
            current_cert.clear();
            current_cert.push_str(line);
            current_cert.push('\n');
        } else if in_cert {
            current_cert.push_str(line);
            current_cert.push('\n');
            if line.contains("-----END CERTIFICATE-----") {
                if let Ok(cert) = reqwest::Certificate::from_pem(current_cert.as_bytes()) {
                    certs.push(cert);
                }
                in_cert = false;
                current_cert.clear();
            }
        }
    }

    if certs.is_empty() {
        return Err("No valid PEM certificates found in file".into());
    }

    Ok(certs)
}

/// Check if the cc-switch reconciler is enabled via env var.
pub fn cc_switch_reconciler_enabled() -> bool {
    let val = std::env::var("HEADROOM_CC_SWITCH_RECONCILE")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    val == "1" || ["true", "yes", "on"].contains(&val.as_str())
}

/// Check if official endpoints should be routed through Headroom.
pub fn cc_switch_route_official() -> bool {
    let val = std::env::var("HEADROOM_CC_SWITCH_ROUTE_OFFICIAL")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    val == "1" || ["true", "yes", "on"].contains(&val.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars are process-global and the test runner is multithreaded;
    /// serialize every test that reads or mutates them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn tls_strict_disabled_default() {
        let _env = env_guard();
        // When unset, strict is NOT disabled (default is strict)
        std::env::remove_var(TLS_STRICT_ENV);
        assert!(!tls_strict_disabled());
    }

    #[test]
    fn tls_strict_disabled_zero() {
        let _env = env_guard();
        std::env::set_var(TLS_STRICT_ENV, "0");
        assert!(tls_strict_disabled());
        std::env::remove_var(TLS_STRICT_ENV);
    }

    #[test]
    fn tls_strict_disabled_false() {
        let _env = env_guard();
        std::env::set_var(TLS_STRICT_ENV, "false");
        assert!(tls_strict_disabled());
        std::env::remove_var(TLS_STRICT_ENV);
    }

    #[test]
    fn tls_strict_disabled_no() {
        let _env = env_guard();
        std::env::set_var(TLS_STRICT_ENV, "no");
        assert!(tls_strict_disabled());
        std::env::remove_var(TLS_STRICT_ENV);
    }

    #[test]
    fn tls_strict_disabled_off() {
        let _env = env_guard();
        std::env::set_var(TLS_STRICT_ENV, "off");
        assert!(tls_strict_disabled());
        std::env::remove_var(TLS_STRICT_ENV);
    }

    #[test]
    fn tls_strict_enabled_one() {
        let _env = env_guard();
        std::env::set_var(TLS_STRICT_ENV, "1");
        assert!(!tls_strict_disabled());
        std::env::remove_var(TLS_STRICT_ENV);
    }

    #[test]
    fn find_ca_bundle_none_when_unset() {
        let _env = env_guard();
        std::env::remove_var("SSL_CERT_FILE");
        std::env::remove_var("REQUESTS_CA_BUNDLE");
        std::env::remove_var(ADDITIVE_CA_VAR);
        assert_eq!(find_ca_bundle(), CaBundleResult::Default);
    }

    #[test]
    fn find_ca_bundle_replacement_ssl_cert_file() {
        let _env = env_guard();
        // Create a temp file to simulate a CA bundle
        let dir = std::env::temp_dir().join("headroom_test_ssl");
        let _ = std::fs::create_dir_all(&dir);
        let cert_path = dir.join("test_ca.pem");
        std::fs::write(&cert_path, "dummy").unwrap();

        std::env::set_var("SSL_CERT_FILE", &cert_path);
        std::env::remove_var("REQUESTS_CA_BUNDLE");
        std::env::remove_var(ADDITIVE_CA_VAR);

        let result = find_ca_bundle();
        assert_eq!(result, CaBundleResult::Replacement(cert_path.clone()));

        // Cleanup
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_dir(&dir);
        std::env::remove_var("SSL_CERT_FILE");
    }

    #[test]
    fn find_ca_bundle_additive_node_extra_ca_certs() {
        let _env = env_guard();
        let dir = std::env::temp_dir().join("headroom_test_ssl_additive");
        let _ = std::fs::create_dir_all(&dir);
        let cert_path = dir.join("test_ca.pem");
        std::fs::write(&cert_path, "dummy").unwrap();

        std::env::remove_var("SSL_CERT_FILE");
        std::env::remove_var("REQUESTS_CA_BUNDLE");
        std::env::set_var(ADDITIVE_CA_VAR, &cert_path);

        let result = find_ca_bundle();
        assert_eq!(result, CaBundleResult::Additive(cert_path.clone()));

        // Cleanup
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_dir(&dir);
        std::env::remove_var(ADDITIVE_CA_VAR);
    }

    #[test]
    fn test_cc_switch_reconciler_disabled_by_default() {
        let _env = env_guard();
        std::env::remove_var("HEADROOM_CC_SWITCH_RECONCILE");
        assert!(!super::cc_switch_reconciler_enabled());
    }

    #[test]
    fn test_cc_switch_reconciler_enabled() {
        let _env = env_guard();
        std::env::set_var("HEADROOM_CC_SWITCH_RECONCILE", "1");
        assert!(super::cc_switch_reconciler_enabled());
        std::env::remove_var("HEADROOM_CC_SWITCH_RECONCILE");
    }

    #[test]
    fn test_cc_switch_route_official_disabled() {
        let _env = env_guard();
        std::env::remove_var("HEADROOM_CC_SWITCH_ROUTE_OFFICIAL");
        assert!(!super::cc_switch_route_official());
    }
}
