//! SSRF guard for client-supplied upstream base URLs (WEB-01).
//!
//! Clients may redirect the proxy's upstream via the `x-headroom-base-url`
//! header (BYOK / custom OpenAI-compatible endpoints). Without validation this
//! lets a caller turn the proxy into a confused deputy — reaching cloud-metadata
//! (`169.254.169.254`) or internal RFC1918 hosts the caller cannot reach
//! directly.
//!
//! Policy, ported from Python's `headroom/proxy/upstream_guard.py`:
//!   * Default: reject destinations that resolve to private, loopback,
//!     link-local, or otherwise non-public addresses. Public hosts
//!     (api.openai.com, api.x.ai, Azure, ...) are allowed so ordinary BYOK keeps
//!     working.
//!   * When `HEADROOM_ALLOWED_BASE_URLS` is set (comma-separated hosts or URLs),
//!     bare hosts permit every safe scheme/port for that host, while URLs permit
//!     only their exact normalized origin. Because that is an explicit operator
//!     choice, allowlisted destinations may point at internal/on-prem endpoints.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Operator allowlist. Set it and the resolve-and-reject default is bypassed
/// for the listed destinations only.
pub const ALLOWED_BASE_URLS_ENV: &str = "HEADROOM_ALLOWED_BASE_URLS";

const SAFE_SCHEMES: &[&str] = &["http", "https", "ws", "wss"];

/// Default port per scheme, for comparing an allowlisted origin against a URL
/// that omitted the port.
fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" | "wss" => 443,
        _ => 80,
    }
}

/// Parsed `HEADROOM_ALLOWED_BASE_URLS`: bare hosts, and exact origins.
struct Allowlist {
    hosts: Vec<String>,
    origins: Vec<(String, String, u16)>,
}

fn allowlisted_destinations() -> Option<Allowlist> {
    let raw = std::env::var(ALLOWED_BASE_URLS_ENV).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let mut hosts = Vec::new();
    let mut origins = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.contains("://") {
            // A full URL pins the exact scheme/host/port triple.
            if let Ok(u) = url::Url::parse(item) {
                if let Some(h) = u.host_str() {
                    let scheme = u.scheme().to_ascii_lowercase();
                    let port = u.port().unwrap_or_else(|| default_port(&scheme));
                    origins.push((scheme, h.to_ascii_lowercase(), port));
                }
            }
        } else {
            // A bare host permits every safe scheme and port for that host.
            // Parsing against a dummy scheme is the cheapest way to strip any
            // port or path the operator wrote.
            if let Ok(u) = url::Url::parse(&format!("http://{item}")) {
                if let Some(h) = u.host_str() {
                    hosts.push(h.to_ascii_lowercase());
                }
            }
        }
    }
    Some(Allowlist { hosts, origins })
}

/// Return true if `addr` is one an outside caller must not reach through us.
///
/// Mirrors Python's `_is_internal_address`. Rust marks several of the relevant
/// predicates unstable (`is_reserved`, `is_unique_local`, `is_global`), so the
/// ranges they cover are spelled out here rather than left out.
pub fn is_internal_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => {
            // An IPv4-mapped address is reachable as its IPv4 self, so judge it
            // by the embedded address rather than by the v6 wrapper.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                is_internal_v4(mapped)
            } else {
                is_internal_v6(v6)
            }
        }
    }
}

fn is_internal_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        // 0.0.0.0/8, "this network".
        || o[0] == 0
        // 100.64.0.0/10, carrier-grade NAT — Python's is_private covers it.
        || (o[0] == 100 && (64..128).contains(&o[1]))
        // 192.0.0.0/24, IETF protocol assignments.
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15, benchmarking.
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 240.0.0.0/4, reserved. Includes 255.255.255.255.
        || o[0] >= 240
}

fn is_internal_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        // fc00::/7, unique local.
        || (s[0] & 0xfe00) == 0xfc00
        // fe80::/10, link local.
        || (s[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32, documentation.
        || (s[0] == 0x2001 && s[1] == 0x0db8)
}

/// Return true if `url` is a safe client-chosen upstream destination.
///
/// In allowlist mode only allowlisted destinations pass. Otherwise the host is
/// resolved and rejected if any resolved address is internal/metadata, which
/// also catches DNS names that point at private space.
///
/// Async because it resolves DNS: the sync equivalent would block a runtime
/// worker on every request that carries the override header.
pub async fn is_safe_upstream_url(url: &url::Url) -> bool {
    let scheme = url.scheme().to_ascii_lowercase();
    if !SAFE_SCHEMES.contains(&scheme.as_str()) {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let port = url.port().unwrap_or_else(|| default_port(&scheme));

    if let Some(allow) = allowlisted_destinations() {
        if allow.hosts.iter().any(|h| h == &host) {
            return true;
        }
        return allow
            .origins
            .iter()
            .any(|(s, h, p)| s == &scheme && h == &host && *p == port);
    }

    // A literal address never reaches the resolver, so check it directly.
    if let Ok(addr) = host.parse::<IpAddr>() {
        return !is_internal_address(addr);
    }

    match tokio::net::lookup_host((host.clone(), port)).await {
        Ok(addrs) => {
            let mut any = false;
            for a in addrs {
                any = true;
                if is_internal_address(a.ip()) {
                    return false;
                }
            }
            any
        }
        // Resolution and connection are separate operations, so allowing a DNS
        // miss here would fail open if the name resolves on the later lookup.
        // Operators can explicitly allowlist split-horizon/internal endpoints.
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn cloud_metadata_and_private_space_are_internal() {
        for ip in [
            "169.254.169.254", // AWS/GCP/Azure metadata
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "0.0.0.0",
            "100.64.0.1",      // CGNAT
            "255.255.255.255",
            "240.0.0.1",       // reserved
        ] {
            assert!(
                is_internal_address(ip.parse().unwrap()),
                "{ip} should be rejected"
            );
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(
                !is_internal_address(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[test]
    fn ipv6_internal_ranges() {
        for ip in ["::1", "::", "fc00::1", "fe80::1", "ff02::1", "2001:db8::1"] {
            assert!(
                is_internal_address(ip.parse().unwrap()),
                "{ip} should be rejected"
            );
        }
    }

    #[test]
    fn ipv4_mapped_v6_is_judged_by_the_embedded_address() {
        // ::ffff:169.254.169.254 reaches metadata just as the bare v4 does.
        assert!(is_internal_address("::ffff:169.254.169.254".parse().unwrap()));
        assert!(!is_internal_address("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn literal_internal_hosts_are_rejected_without_dns() {
        assert!(!is_safe_upstream_url(&u("http://169.254.169.254/latest/meta-data/")).await);
        assert!(!is_safe_upstream_url(&u("http://127.0.0.1:8788")).await);
        assert!(!is_safe_upstream_url(&u("https://[::1]/v1")).await);
    }

    #[tokio::test]
    async fn unsafe_schemes_are_rejected() {
        assert!(!is_safe_upstream_url(&u("file:///etc/passwd")).await);
        assert!(!is_safe_upstream_url(&u("gopher://8.8.8.8/")).await);
    }

    #[tokio::test]
    async fn public_literal_is_allowed() {
        assert!(is_safe_upstream_url(&u("https://8.8.8.8/v1")).await);
    }
}
