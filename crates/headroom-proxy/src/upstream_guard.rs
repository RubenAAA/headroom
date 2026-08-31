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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Operator allowlist. Set it and the resolve-and-reject default is bypassed
/// for the listed destinations only.
pub const ALLOWED_BASE_URLS_ENV: &str = "HEADROOM_ALLOWED_BASE_URLS";

/// How long a caller-supplied host gets to resolve before it is rejected.
///
/// Generous for a real resolver, short enough that a hostile one cannot park a
/// request task. Resolution happens before the request is forwarded, so this
/// is not on any hot path a legitimate caller notices.
const RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const SAFE_SCHEMES: &[&str] = &["http", "https", "ws", "wss"];

fn normalized_host(url: &url::Url) -> Option<String> {
    match url.host()? {
        url::Host::Domain(domain) => Some(domain.to_ascii_lowercase()),
        url::Host::Ipv4(address) => Some(address.to_string()),
        url::Host::Ipv6(address) => Some(address.to_string()),
    }
}

/// A caller-supplied upstream paired with the exact addresses approved by the
/// SSRF policy.
///
/// The URL keeps the original hostname for HTTP `Host` and TLS SNI. The socket
/// addresses are installed into a request-scoped reqwest client with
/// `resolve_to_addrs`, so connection establishment cannot perform a second DNS
/// lookup and swap in a private address after validation.
#[derive(Debug, Clone)]
pub struct ResolvedCallerUpstream {
    url: url::Url,
    host: String,
    addresses: Vec<SocketAddr>,
}

impl ResolvedCallerUpstream {
    /// Resolve and validate a caller-controlled destination.
    ///
    /// Every returned DNS answer must be acceptable. Filtering a mixed answer
    /// set would make policy depend on reqwest's address-selection order.
    pub async fn resolve(url: url::Url) -> Option<Self> {
        let scheme = url.scheme().to_ascii_lowercase();
        if !SAFE_SCHEMES.contains(&scheme.as_str()) {
            return None;
        }
        let host = normalized_host(&url)?;
        let port = url.port().unwrap_or_else(|| default_port(&scheme));

        let allow_internal = if let Some(allow) = allowlisted_destinations() {
            let allowed_host = allow.hosts.iter().any(|candidate| candidate == &host);
            let allowed_origin = allow
                .origins
                .iter()
                .any(|(s, h, p)| s == &scheme && h == &host && *p == port);
            if !allowed_host && !allowed_origin {
                return None;
            }
            true
        } else {
            false
        };

        let addresses = if let Ok(address) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(address, port)]
        } else {
            let resolved = tokio::time::timeout(
                RESOLVE_TIMEOUT,
                tokio::net::lookup_host((host.clone(), port)),
            )
            .await;
            match resolved {
                Ok(Ok(addresses)) => addresses.collect::<Vec<_>>(),
                Ok(Err(error)) => {
                    tracing::warn!(
                        event = "upstream_guard_resolve_failed",
                        host = %host,
                        error = %error,
                        "rejecting a caller-supplied upstream whose host did not resolve"
                    );
                    return None;
                }
                Err(_) => {
                    tracing::warn!(
                        event = "upstream_guard_resolve_timeout",
                        host = %host,
                        timeout_ms = RESOLVE_TIMEOUT.as_millis() as u64,
                        "rejecting a caller-supplied upstream whose host did not resolve in time"
                    );
                    return None;
                }
            }
        };

        if !resolved_addresses_allowed(&addresses, allow_internal) {
            return None;
        }

        Some(Self {
            url,
            host,
            addresses,
        })
    }

    pub fn url(&self) -> &url::Url {
        &self.url
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

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
                if let Some(h) = normalized_host(&u) {
                    let scheme = u.scheme().to_ascii_lowercase();
                    let port = u.port().unwrap_or_else(|| default_port(&scheme));
                    origins.push((scheme, h, port));
                }
            }
        } else {
            // A bare host permits every safe scheme and port for that host.
            // Parsing against a dummy scheme is the cheapest way to strip any
            // port or path the operator wrote.
            if let Ok(u) = url::Url::parse(&format!("http://{item}")) {
                if let Some(h) = normalized_host(&u) {
                    hosts.push(h);
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

/// The IPv4 address a transition-mechanism v6 address actually reaches, if any.
///
/// `to_ipv4_mapped` covers `::ffff:0:0/96` and nothing else, so an address that
/// carries an IPv4 destination in some other encoding reads as an ordinary
/// global v6 address and skips the v4 rules entirely. `2002:a9fe:a9fe::1` is a
/// 6to4 wrapper around `169.254.169.254` — the cloud metadata endpoint — and
/// would otherwise pass.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    // 2002::/16, 6to4: the IPv4 address sits in the next two groups.
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::from(((s[1] as u32) << 16) | s[2] as u32));
    }
    // 2001:0000::/32, Teredo: the client IPv4 is the last two groups, stored
    // inverted.
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return Some(Ipv4Addr::from(!(((s[6] as u32) << 16) | s[7] as u32)));
    }
    // 64:ff9b::/96, NAT64 well-known prefix.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return Some(Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32));
    }
    None
}

fn is_internal_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    // Judge a transition address by where it actually lands.
    if let Some(v4) = embedded_v4(v6) {
        return is_internal_v4(v4);
    }
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

fn resolved_addresses_allowed(addresses: &[SocketAddr], allow_internal: bool) -> bool {
    !addresses.is_empty()
        && (allow_internal
            || addresses
                .iter()
                .all(|address| !is_internal_address(address.ip())))
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
    ResolvedCallerUpstream::resolve(url.clone()).await.is_some()
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
            "100.64.0.1", // CGNAT
            "255.255.255.255",
            "240.0.0.1", // reserved
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
    fn a_mixed_dns_answer_is_rejected_as_a_unit() {
        let addresses = [
            "8.8.8.8:443".parse().unwrap(),
            "169.254.169.254:443".parse().unwrap(),
        ];
        assert!(!resolved_addresses_allowed(&addresses, false));
    }

    #[test]
    fn an_explicit_allowlist_may_pin_internal_addresses() {
        let addresses = ["127.0.0.1:8788".parse().unwrap()];
        assert!(resolved_addresses_allowed(&addresses, true));
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
        assert!(is_internal_address(
            "::ffff:169.254.169.254".parse().unwrap()
        ));
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
        assert!(is_safe_upstream_url(&u("https://[2606:4700::1111]/v1")).await);
    }

    /// A v6 address can carry an IPv4 destination in encodings
    /// `to_ipv4_mapped` does not cover. Each of these reaches an address the
    /// v4 rules already reject, so the guard must judge them by where they
    /// land rather than by the wrapper.
    #[test]
    fn transition_addresses_are_judged_by_their_embedded_v4() {
        // 6to4 around the cloud metadata endpoint (169.254.169.254).
        assert!(is_internal_address("2002:a9fe:a9fe::1".parse().unwrap()));
        // 6to4 around RFC1918.
        assert!(is_internal_address("2002:0a00:0001::1".parse().unwrap()));
        // NAT64 well-known prefix around metadata.
        assert!(is_internal_address("64:ff9b::a9fe:a9fe".parse().unwrap()));
        // Teredo carrying 10.0.0.1, stored inverted (0xf5fffffe).
        assert!(is_internal_address(
            "2001:0:0:0:0:0:f5ff:fffe".parse().unwrap()
        ));
    }

    /// The decoding must not swallow ordinary public v6 traffic: 6to4 around a
    /// public v4 address is still public, and an unrelated 2001: prefix is not
    /// Teredo.
    #[test]
    fn transition_addresses_to_public_space_still_pass() {
        // 6to4 around 8.8.8.8.
        assert!(!is_internal_address("2002:0808:0808::1".parse().unwrap()));
        // 2001:4860::/32 is Google, not Teredo — only 2001:0000::/32 is.
        assert!(!is_internal_address(
            "2001:4860:4860::8888".parse().unwrap()
        ));
    }
}
