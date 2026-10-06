//! Startup bind policy: refuse a token-less non-loopback bind.
//!
//! Port of upstream `headroom/proxy/bind_policy.py` (commit `ee731d7e`,
//! findings 01-F1/01-F2/01-F5/01-F9). A non-loopback bind with no
//! `HEADROOM_PROXY_TOKEN` used to log a warning and then serve the `/v1/*`
//! relay, with the operator's upstream credentials, to the whole network.
//! Now the proxy refuses to start unless the operator acknowledges the open
//! bind explicitly. The evaluation is pure (`evaluate`) so tests can cover
//! the matrix without binding sockets; `enforce_bind_policy` wires it into
//! `main.rs` before `serve`.

use std::net::SocketAddr;

/// Env var acknowledging an unauthenticated non-loopback bind. Named for
/// the decision it records: the `/v1/*` data plane stays open.
pub const OPEN_BIND_ACK_ENV: &str = "HEADROOM_ALLOW_UNAUTHENTICATED_BIND";

/// Accepted truthy values for [`OPEN_BIND_ACK_ENV`], matching upstream.
const TRUE_VALUES: &[&str] = &["1", "true", "yes", "on"];

/// Outcome of [`evaluate_bind_policy`].
pub struct BindPolicy {
    /// The bind host as printed in log lines.
    pub host: String,
    /// True when the bind reaches beyond loopback, unspecified included.
    pub open_bind: bool,
    /// True when the operator acknowledged an open bind via [`OPEN_BIND_ACK_ENV`].
    pub acknowledged: bool,
}

impl BindPolicy {
    /// Allow unless this is an unacknowledged open bind without a token.
    pub fn allow(&self, token_set: bool) -> bool {
        if !self.open_bind || token_set {
            return true;
        }
        self.acknowledged
    }

    /// Human-readable outcome, logged at startup either way.
    pub fn describe(&self, token_set: bool) -> String {
        if !self.open_bind {
            return format!("bind {}: inbound auth OK", self.host);
        }
        if token_set {
            return format!("bind {}: inbound auth OK", self.host);
        }
        if self.acknowledged {
            return format!(
                "bind {}: non-loopback bind with NO HEADROOM_PROXY_TOKEN — \
                 /v1/* is UNAUTHENTICATED; acknowledged via {OPEN_BIND_ACK_ENV}=1",
                self.host
            );
        }
        format!(
            "Refusing to bind {:?}: the proxy would serve its /v1/* data-plane \
             routes to every peer on the network without authentication, relaying \
             the operator's upstream credentials. Set HEADROOM_PROXY_TOKEN, bind \
             loopback, or acknowledge the open bind with {OPEN_BIND_ACK_ENV}=1.",
            self.host
        )
    }
}

/// Pure bind-policy evaluation. `listen` is the socket the server would
/// bind. Only loopback is closed: unspecified (`0.0.0.0`, `::`) listens on
/// every interface, the exact shape upstream refuses (systemd units,
/// `docker run -p`).
pub fn evaluate_bind_policy(listen: &SocketAddr, acknowledged: bool) -> BindPolicy {
    let ip = listen.ip();
    let open_bind = !ip.is_loopback();
    BindPolicy {
        host: ip.to_string(),
        open_bind,
        acknowledged,
    }
}

/// Read [`OPEN_BIND_ACK_ENV`] the way upstream does: case-insensitive,
/// surrounding whitespace ignored.
pub fn acknowledged_from_env() -> bool {
    std::env::var(OPEN_BIND_ACK_ENV)
        .map(|v| TRUE_VALUES.contains(&v.trim().to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Enforce the policy before `serve`: logs the outcome and returns the
/// refusal message when the proxy must not start.
pub fn enforce_bind_policy(listen: &SocketAddr, token_set: bool) -> Result<(), String> {
    let policy = evaluate_bind_policy(listen, acknowledged_from_env());
    let message = policy.describe(token_set);
    if policy.allow(token_set) {
        if policy.open_bind {
            tracing::warn!(event = "proxy_open_bind", host = %policy.host, "{message}");
        }
        Ok(())
    } else {
        Err(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_bind_needs_no_token() {
        for host in ["127.0.0.1:8787", "[::1]:8787"] {
            let p = evaluate_bind_policy(&addr(host), false);
            assert!(!p.open_bind, "{host}");
            assert!(p.allow(false));
            assert!(p.describe(false).contains("inbound auth OK"));
        }
    }

    #[test]
    fn unspecified_bind_is_open() {
        for host in ["0.0.0.0:8787", "[::]:8787"] {
            let p = evaluate_bind_policy(&addr(host), false);
            assert!(p.open_bind, "{host}");
            assert!(!p.allow(false), "{host}");
            assert!(p.allow(true), "{host}");
        }
    }

    #[test]
    fn non_loopback_with_token_is_allowed() {
        let p = evaluate_bind_policy(&addr("192.168.1.10:8787"), false);
        assert!(p.open_bind);
        assert!(p.allow(true));
        assert!(p.describe(true).contains("inbound auth OK"));
    }

    #[test]
    fn non_loopback_without_token_is_refused() {
        let p = evaluate_bind_policy(&addr("192.168.1.10:8787"), false);
        assert!(!p.allow(false));
        let msg = p.describe(false);
        assert!(msg.contains("Refusing to bind"), "{msg}");
        assert!(msg.contains(OPEN_BIND_ACK_ENV), "{msg}");
    }

    #[test]
    fn acknowledgement_allows_but_warns() {
        let p = evaluate_bind_policy(&addr("192.168.1.10:8787"), true);
        assert!(p.allow(false));
        let msg = p.describe(false);
        assert!(msg.contains("UNAUTHENTICATED"), "{msg}");
        assert!(msg.contains(OPEN_BIND_ACK_ENV), "{msg}");
    }

    #[test]
    fn ack_env_parsing_matches_upstream() {
        for truthy in ["1", "true", "TRUE", " yes ", "on", "On"] {
            assert!(
                TRUE_VALUES.contains(&truthy.trim().to_ascii_lowercase().as_str()),
                "{truthy}"
            );
        }
        for falsy in ["", "0", "false", "no", "2"] {
            assert!(
                !TRUE_VALUES.contains(&falsy.trim().to_ascii_lowercase().as_str()),
                "{falsy}"
            );
        }
    }

    #[test]
    fn enforce_refuses_open_tokenless_bind() {
        // Env-dependent: only assert the refusal shape when the ack var is
        // unset in this worker; otherwise the ack path above covers it.
        if std::env::var(OPEN_BIND_ACK_ENV).is_ok() {
            return;
        }
        let err = enforce_bind_policy(&addr("192.168.1.10:8787"), false).unwrap_err();
        assert!(err.contains("Refusing to bind"), "{err}");
        assert!(enforce_bind_policy(&addr("127.0.0.1:8787"), false).is_ok());
    }
}
