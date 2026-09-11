//! Loopback-only access guard for /debug/* endpoints.
//!
//! Pure validation functions for determining if a client address or Host header
//! represents a loopback interface. The FastAPI `require_loopback` dependency
//! stays in Python; only the IP/hostname logic is ported here.

use std::net::IpAddr;

/// Canonical loopback literal set (for backwards compatibility).
pub const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "::1", "localhost"];

/// Return true if `host` represents a loopback interface.
///
/// `None` is treated as loopback (covers TestClient / UDS-style requests).
/// `"localhost"` is special-cased as a string since it is not a valid IP literal.
/// Comparison is case-insensitive (RFC 4343).
pub fn is_loopback_host(host: Option<&str>) -> bool {
    match host {
        None => true,
        Some(h) if h.eq_ignore_ascii_case("localhost") => true,
        Some(h) => {
            let addr: IpAddr = match h.parse() {
                Ok(a) => a,
                Err(_) => return false,
            };
            match addr {
                IpAddr::V4(v4) => v4.is_loopback(),
                IpAddr::V6(v6) => {
                    if let Some(mapped) = v6.to_ipv4_mapped() {
                        mapped.is_loopback()
                    } else {
                        v6.is_loopback()
                    }
                }
            }
        }
    }
}

/// Return true if a `Host:` header names a loopback address.
///
/// The header can include a port (`127.0.0.1:8787`, `[::1]:8787`,
/// `localhost:8787`) and uses bracket notation for raw IPv6 literals per RFC 3986.
/// Missing / empty headers return `False` — a real local browser or CLI always
/// sets `Host:`.
pub fn is_loopback_host_header(header_value: Option<&str>) -> bool {
    let candidate = match header_value {
        Some(v) => v.trim(),
        None => return false,
    };
    if candidate.is_empty() {
        return false;
    }

    let host_part = if candidate.starts_with('[') {
        // Bracketed IPv6: [::1] or [::1]:8787
        match candidate.find(']') {
            Some(closing) => &candidate[1..closing],
            None => return false,
        }
    } else if candidate.matches(':').count() == 1 {
        // Single colon = host:port for IPv4 / hostname
        match candidate.rsplit_once(':') {
            Some((host, _)) => host,
            None => candidate,
        }
    } else {
        candidate
    };

    is_loopback_host(Some(host_part))
}

/// Return whether `Host:` contains an IPv4 or bracketed IPv6 literal.
///
/// Dashboard clients may use a non-loopback server address, but retaining an
/// IP-literal Host requirement prevents DNS-rebinding requests from using an
/// attacker-controlled hostname. Ports are accepted in normal HTTP forms.
/// Port of upstream `loopback_guard.is_ip_literal_host_header`.
pub fn is_ip_literal_host_header(header_value: Option<&str>) -> bool {
    let candidate = match header_value {
        Some(v) => v.trim(),
        None => return false,
    };
    if candidate.is_empty() || candidate.contains('/') || candidate.contains('@') {
        return false;
    }
    if let Some(rest) = candidate.strip_prefix('[') {
        // Bracketed IPv6: [::1] or [::1]:8787 — exactly one bracket pair,
        // optional numeric port suffix.
        if candidate.matches('[').count() != 1 {
            return false;
        }
        let Some(closing) = rest.find(']') else {
            return false;
        };
        if rest.matches(']').count() != 1 {
            return false;
        }
        let host_part = &rest[..closing];
        let suffix = &rest[closing + 1..];
        if !suffix.is_empty() {
            let Some(port) = suffix.strip_prefix(':') else {
                return false;
            };
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
        }
        return host_part
            .parse::<IpAddr>()
            .is_ok_and(|a| matches!(a, IpAddr::V6(_)));
    }
    let host_part = if candidate.matches(':').count() == 1 {
        match candidate.rsplit_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return false;
                }
                host
            }
            None => candidate,
        }
    } else {
        candidate
    };
    host_part
        .parse::<IpAddr>()
        .is_ok_and(|a| matches!(a, IpAddr::V4(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_loopback_host ──────────────────────────────────────────

    #[test]
    fn none_is_loopback() {
        assert!(is_loopback_host(None));
    }

    #[test]
    fn localhost_is_loopback() {
        assert!(is_loopback_host(Some("localhost")));
        assert!(is_loopback_host(Some("LOCALHOST")));
        assert!(is_loopback_host(Some("LocalHost")));
    }

    #[test]
    fn ipv4_loopback() {
        assert!(is_loopback_host(Some("127.0.0.1")));
        assert!(is_loopback_host(Some("127.0.0.2")));
    }

    #[test]
    fn ipv4_not_loopback() {
        assert!(!is_loopback_host(Some("10.0.0.1")));
        assert!(!is_loopback_host(Some("8.8.8.8")));
    }

    #[test]
    fn ipv6_loopback() {
        assert!(is_loopback_host(Some("::1")));
    }

    #[test]
    fn ipv6_not_loopback() {
        assert!(!is_loopback_host(Some("fd00::1")));
    }

    #[test]
    fn ipv4_mapped_ipv6() {
        assert!(is_loopback_host(Some("::ffff:127.0.0.1")));
    }

    #[test]
    fn ipv4_mapped_not_loopback() {
        assert!(!is_loopback_host(Some("::ffff:10.0.0.1")));
    }

    #[test]
    fn malformed_is_not_loopback() {
        assert!(!is_loopback_host(Some("not-an-ip")));
        assert!(!is_loopback_host(Some("")));
    }

    // ── is_loopback_host_header ──────────────────────────────────

    #[test]
    fn none_header_is_false() {
        assert!(!is_loopback_host_header(None));
    }

    #[test]
    fn empty_header_is_false() {
        assert!(!is_loopback_host_header(Some("")));
        assert!(!is_loopback_host_header(Some("  ")));
    }

    #[test]
    fn bare_localhost() {
        assert!(is_loopback_host_header(Some("localhost")));
    }

    #[test]
    fn localhost_with_port() {
        assert!(is_loopback_host_header(Some("localhost:8787")));
    }

    #[test]
    fn ipv4_with_port() {
        assert!(is_loopback_host_header(Some("127.0.0.1:8787")));
    }

    #[test]
    fn ipv6_bracketed() {
        assert!(is_loopback_host_header(Some("[::1]")));
        assert!(is_loopback_host_header(Some("[::1]:8787")));
    }

    #[test]
    fn non_loopback_header() {
        assert!(!is_loopback_host_header(Some("attacker.example")));
        assert!(!is_loopback_host_header(Some("10.0.0.1:8080")));
    }

    #[test]
    fn malformed_bracket_is_false() {
        assert!(!is_loopback_host_header(Some("[::1")));
    }

    // ── is_ip_literal_host_header ─────────────────────────────────

    #[test]
    fn ip_literal_v4() {
        assert!(is_ip_literal_host_header(Some("10.0.0.5")));
        assert!(is_ip_literal_host_header(Some("10.0.0.5:8787")));
    }

    #[test]
    fn ip_literal_v6_bracketed() {
        assert!(is_ip_literal_host_header(Some("[fd00::1]")));
        assert!(is_ip_literal_host_header(Some("[fd00::1]:8787")));
    }

    #[test]
    fn ip_literal_rejects_names_and_garbage() {
        assert!(!is_ip_literal_host_header(None));
        assert!(!is_ip_literal_host_header(Some("")));
        // Hostnames are not literals, even loopback ones (DNS-rebinding).
        assert!(!is_ip_literal_host_header(Some("localhost")));
        assert!(!is_ip_literal_host_header(Some("localhost:8787")));
        assert!(!is_ip_literal_host_header(Some("attacker.example")));
        assert!(!is_ip_literal_host_header(Some("10.0.0.5:notaport")));
        assert!(!is_ip_literal_host_header(Some("10.0.0.5/24")));
        assert!(!is_ip_literal_host_header(Some("user@10.0.0.5")));
        // Unbracketed IPv6 is ambiguous with host:port — rejected.
        assert!(!is_ip_literal_host_header(Some("::1")));
        assert!(!is_ip_literal_host_header(Some("[::1")));
    }
}
