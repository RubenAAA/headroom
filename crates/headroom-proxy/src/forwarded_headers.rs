//! Trusted-gateway gate for `X-Forwarded-*` headers, plus the dashboard
//! remote-access gate.
//!
//! Pure validation and parsing functions for CIDR allow-lists and IP
//! normalization, client-IP resolution behind trusted gateways, and the
//! same-origin + CIDR check that authorizes sensitive dashboard metadata
//! for non-loopback callers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A parsed CIDR network (IPv4 or IPv6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpCidr {
    addr: IpAddr,
    prefix: u8,
}

impl std::fmt::Display for IpCidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl std::str::FromStr for IpCidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr_str, prefix_str) = s
            .rsplit_once('/')
            .ok_or_else(|| format!("missing /prefix in {s}"))?;

        let prefix: u8 = prefix_str
            .parse()
            .map_err(|_| format!("invalid prefix in {s}"))?;

        let addr: IpAddr = addr_str
            .parse()
            .map_err(|_| format!("invalid address in {s}"))?;

        match addr {
            IpAddr::V4(_) if prefix > 32 => Err(format!("prefix > 32 in {s}")),
            IpAddr::V6(_) if prefix > 128 => Err(format!("prefix > 128 in {s}")),
            _ => {
                // Normalize: zero out host bits
                let normalized = Self::normalize(addr, prefix);
                Ok(IpCidr {
                    addr: normalized,
                    prefix,
                })
            }
        }
    }
}

impl IpCidr {
    fn normalize(addr: IpAddr, prefix: u8) -> IpAddr {
        match addr {
            IpAddr::V4(v4) => {
                let bits = u32::from(v4);
                let mask = if prefix == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - prefix)
                };
                IpAddr::V4(Ipv4Addr::from(bits & mask))
            }
            IpAddr::V6(v6) => {
                let bits = u128::from(v6);
                let mask = if prefix == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - prefix)
                };
                IpAddr::V6(Ipv6Addr::from(bits & mask))
            }
        }
    }

    /// Check if an IP address belongs to this CIDR.
    pub fn contains(&self, addr: &IpAddr) -> bool {
        match (&self.addr, addr) {
            (IpAddr::V4(net), IpAddr::V4(host)) => {
                let net_bits = u32::from(*net);
                let host_bits = u32::from(*host);
                let mask = if self.prefix == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                (net_bits & mask) == (host_bits & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(host)) => {
                let net_bits = u128::from(*net);
                let host_bits = u128::from(*host);
                let mask = if self.prefix == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                (net_bits & mask) == (host_bits & mask)
            }
            _ => false,
        }
    }
}

/// Parse a comma-separated CIDR list. Empty / whitespace → empty list.
pub fn parse_cidr_list(raw: &str) -> Result<Vec<IpCidr>, String> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut nets = Vec::new();
    for chunk in raw.split(',') {
        let entry = chunk.trim();
        if entry.is_empty() {
            continue;
        }
        nets.push(entry.parse()?);
    }
    Ok(nets)
}

/// Environment variable holding the trusted-gateway CIDR allow-list.
pub const TRUSTED_GATEWAY_CIDRS_ENV: &str = "HEADROOM_PROXY_TRUSTED_GATEWAY_CIDRS";
/// Environment variable holding the Dashboard client CIDR allow-list.
///
/// Kept SEPARATE from the gateway list: trusting a proxy to report a client's
/// real IP is a different decision from trusting a client to reach the
/// Dashboard, and merging them would silently widen one to the other.
pub const TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV: &str =
    "HEADROOM_PROXY_TRUSTED_DASHBOARD_CLIENT_CIDRS";

/// Load and parse the trusted-gateway CIDR allow-list.
pub fn load_trusted_gateway_cidrs(raw: &str) -> Result<Vec<IpCidr>, String> {
    parse_cidr_list(raw)
}

/// Parse the Dashboard client CIDR allow-list.
///
/// A malformed entry is an error rather than an empty list: silently yielding
/// no CIDRs would turn a typo into "allow nothing" (or, depending on the
/// caller, "allow everything"), and an operator's misconfigured allow-list
/// should fail loudly.
pub fn load_trusted_dashboard_client_cidrs(raw: &str) -> Result<Vec<IpCidr>, String> {
    parse_cidr_list(raw)
        .map_err(|e| format!("Invalid {TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV} entry: {e}"))
}

/// Read the Dashboard client allow-list from the environment.
pub fn load_trusted_dashboard_client_cidrs_from_env() -> Result<Vec<IpCidr>, String> {
    load_trusted_dashboard_client_cidrs(
        &std::env::var(TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV).unwrap_or_default(),
    )
}

/// Parse `host` into an IPv4/IPv6 address, unmapping `::ffff:*`.
///
/// IPv4-mapped IPv6 addresses are normalized to their underlying IPv4 form.
/// Returns `None` on malformed input.
pub fn normalize_ip(host: &str) -> Option<IpAddr> {
    let addr: IpAddr = host.parse().ok()?;
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).or(Some(addr)),
        IpAddr::V4(_) => Some(addr),
    }
}

/// Return true iff `peer_host` is inside any of the allow-list CIDRs.
///
/// Empty allow-list → always False (strict-secure default).
/// `None` peer → False.
pub fn peer_is_trusted_gateway(peer_host: Option<&str>, cidrs: &[IpCidr]) -> bool {
    if cidrs.is_empty() {
        return false;
    }
    let peer = match peer_host {
        Some(p) => p,
        None => return false,
    };
    let addr = match normalize_ip(peer) {
        Some(a) => a,
        None => return false,
    };
    cidrs.iter().any(|net| net.contains(&addr))
}

/// Return the leftmost element of a comma-separated header value.
///
/// `X-Forwarded-For: client, proxy1, proxy2` → `"client"`.
pub fn header_first(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    match value.split_once(',') {
        Some((head, _)) => head.trim().to_string(),
        None => value.trim().to_string(),
    }
}

/// Resolve the client IP from request headers, considering trusted gateways.
///
/// When the peer address is a trusted gateway, we trust `X-Forwarded-For`.
/// Otherwise, we use the direct peer address.
pub fn resolve_client_ip(
    peer_addr: Option<&str>,
    headers: &axum::http::HeaderMap,
    trusted_cidrs: &[IpCidr],
) -> String {
    if peer_is_trusted_gateway(peer_addr, trusted_cidrs) {
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(val) = xff.to_str() {
                let ip = header_first(val);
                if !ip.is_empty() {
                    return ip;
                }
            }
        }
    }
    peer_addr.unwrap_or("unknown").to_string()
}

/// A normalized HTTP(S) origin: (scheme, host, port).
fn normalized_http_origin(value: &str) -> Option<(String, String, u16)> {
    let url = url::Url::parse(value.trim()).ok()?;
    let scheme = url.scheme().to_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = url
        .port()
        .unwrap_or(if scheme == "http" { 80 } else { 443 });
    Some((scheme, host, port))
}

/// Accept no browser provenance, otherwise require same-origin headers.
///
/// Native CLI clients usually send neither `Origin` nor `Referer`, so
/// absence remains valid. If either is present it must identify this exact
/// scheme/host/port, so a victim's browser cannot be used to read sensitive
/// metadata cross-origin. Port of upstream
/// `_request_has_same_origin_or_no_provenance`.
pub fn has_same_origin_or_no_provenance(
    host_header: &str,
    origin: Option<&str>,
    referer: Option<&str>,
    scheme: &str,
) -> bool {
    let expected = match normalized_http_origin(&format!("{scheme}://{host_header}")) {
        Some(o) => o,
        None => return false,
    };
    for header_value in [origin, referer].into_iter().flatten() {
        if header_value.is_empty() {
            continue;
        }
        if normalized_http_origin(header_value) != Some(expected.clone()) {
            return false;
        }
    }
    true
}

/// Authorize sensitive dashboard metadata without widening admin access.
///
/// Loopback callers (plus trusted-gateway peers, the containerized-dashboard
/// case) pass outright. Anyone else must clear three gates: an IP-literal
/// `Host:` (DNS-rebinding defence), same-origin-or-no-provenance, and a
/// client IP inside the operator-configured dashboard CIDRs (strict-secure
/// default: empty list allows nothing). Port of upstream
/// `_request_can_view_dashboard_metadata`.
pub fn can_view_dashboard_metadata(
    peer_host: Option<&str>,
    headers: &axum::http::HeaderMap,
    gateway_cidrs: &[IpCidr],
    dashboard_cidrs: &[IpCidr],
) -> bool {
    use crate::loopback_guard::{
        is_ip_literal_host_header, is_loopback_host, is_loopback_host_header,
    };

    let host_header = headers.get("host").and_then(|v| v.to_str().ok());

    // Loopback by peer IP and Host header, or gateway-equivalent peer.
    // The Host-header gate always applies (DNS-rebinding defence).
    if is_loopback_host_header(host_header) {
        if is_loopback_host(peer_host) {
            return true;
        }
        if peer_is_trusted_gateway(peer_host, gateway_cidrs) {
            return true;
        }
    }

    let Some(host_header) = host_header.filter(|h| !h.trim().is_empty()) else {
        return false;
    };
    if !is_ip_literal_host_header(Some(host_header)) {
        return false;
    }
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // Scheme behind a TLS terminator comes from the forwarded proto, but
    // only when the peer is a trusted gateway; otherwise only XFF-trusted
    // input would be attacker-controlled. Direct peers serve plain HTTP.
    let scheme = if peer_is_trusted_gateway(peer_host, gateway_cidrs) {
        header_str("x-forwarded-proto").unwrap_or("http")
    } else {
        "http"
    };
    if !has_same_origin_or_no_provenance(
        host_header,
        header_str("origin"),
        header_str("referer"),
        scheme,
    ) {
        return false;
    }
    peer_is_trusted_gateway(
        Some(resolve_client_ip(peer_host, headers, gateway_cidrs).as_str()),
        dashboard_cidrs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CIDR parsing ──────────────────────────────────────────────

    #[test]
    fn load_cidrs_empty_string_is_empty() {
        assert!(load_trusted_gateway_cidrs("").unwrap().is_empty());
        assert!(load_trusted_gateway_cidrs("   ").unwrap().is_empty());
    }

    #[test]
    fn load_cidrs_single() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert_eq!(cidrs.len(), 1);
        assert_eq!(cidrs[0].to_string(), "10.0.0.0/8");
    }

    #[test]
    fn load_cidrs_multiple() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8,172.16.0.0/12,fd00::/8").unwrap();
        assert_eq!(cidrs.len(), 3);
    }

    #[test]
    fn load_cidrs_whitespace_tolerant() {
        let cidrs = load_trusted_gateway_cidrs(" 10.0.0.0/8 , 172.16.0.0/12 ").unwrap();
        assert_eq!(cidrs.len(), 2);
    }

    #[test]
    fn load_cidrs_trailing_comma_tolerant() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8,").unwrap();
        assert_eq!(cidrs.len(), 1);
    }

    #[test]
    fn load_cidrs_host_bits_normalized() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.1/8").unwrap();
        assert_eq!(cidrs[0].to_string(), "10.0.0.0/8");
    }

    #[test]
    fn load_cidrs_malformed_raises() {
        assert!(load_trusted_gateway_cidrs("not-a-cidr").is_err());
    }

    #[test]
    fn load_cidrs_partial_malformed_raises() {
        assert!(load_trusted_gateway_cidrs("10.0.0.0/8,not-a-cidr,fd00::/8").is_err());
    }

    // ── Membership check ──────────────────────────────────────────

    #[test]
    fn peer_membership_empty_allowlist_is_false() {
        assert!(!peer_is_trusted_gateway(Some("10.0.0.5"), &[]));
    }

    #[test]
    fn peer_membership_none_peer_is_false() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(!peer_is_trusted_gateway(None, &cidrs));
    }

    #[test]
    fn peer_membership_in_v4_cidr() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(peer_is_trusted_gateway(Some("10.0.0.5"), &cidrs));
    }

    #[test]
    fn peer_membership_outside_v4_cidr() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(!peer_is_trusted_gateway(Some("8.8.8.8"), &cidrs));
    }

    #[test]
    fn peer_membership_in_v6_cidr() {
        let cidrs = load_trusted_gateway_cidrs("fd00::/8").unwrap();
        assert!(peer_is_trusted_gateway(Some("fd00::1"), &cidrs));
    }

    #[test]
    fn peer_membership_v4_mapped_v6() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(peer_is_trusted_gateway(Some("::ffff:10.0.0.1"), &cidrs));
    }

    #[test]
    fn peer_membership_v4_not_in_v6_only_cidr() {
        let cidrs = load_trusted_gateway_cidrs("fd00::/8").unwrap();
        assert!(!peer_is_trusted_gateway(Some("10.0.0.5"), &cidrs));
    }

    #[test]
    fn peer_membership_v6_not_in_v4_only_cidr() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(!peer_is_trusted_gateway(Some("fd00::1"), &cidrs));
    }

    #[test]
    fn peer_membership_evaluates_all_cidrs() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8,172.16.0.0/12,fd00::/8").unwrap();
        assert!(peer_is_trusted_gateway(Some("172.16.5.5"), &cidrs));
        assert!(peer_is_trusted_gateway(Some("fd00::beef"), &cidrs));
    }

    #[test]
    fn peer_membership_malformed_peer_is_false() {
        let cidrs = load_trusted_gateway_cidrs("10.0.0.0/8").unwrap();
        assert!(!peer_is_trusted_gateway(Some("not-an-ip"), &cidrs));
    }

    // ── header_first ──────────────────────────────────────────────

    #[test]
    fn header_first_single() {
        assert_eq!(header_first("203.0.113.7"), "203.0.113.7");
    }

    #[test]
    fn header_first_comma_separated() {
        assert_eq!(
            header_first("203.0.113.7, 10.0.0.99, 10.0.0.5"),
            "203.0.113.7"
        );
    }

    #[test]
    fn header_first_empty() {
        assert_eq!(header_first(""), "");
    }

    #[test]
    fn header_first_whitespace() {
        assert_eq!(header_first("  203.0.113.7  "), "203.0.113.7");
    }

    // ── normalize_ip ──────────────────────────────────────────────

    #[test]
    fn normalize_ip_v4() {
        let addr = normalize_ip("10.0.0.1").unwrap();
        assert_eq!(addr, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn normalize_ip_v6_loopback() {
        let addr = normalize_ip("::1").unwrap();
        assert_eq!(addr, IpAddr::V6(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn normalize_ip_v4_mapped() {
        let addr = normalize_ip("::ffff:10.0.0.1").unwrap();
        assert_eq!(addr, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn normalize_ip_malformed() {
        assert!(normalize_ip("not-an-ip").is_none());
    }

    // ─── Dashboard client allow-list (upstream addition) ─────────────────

    #[test]
    fn dashboard_cidr_list_matches_python() {
        assert_eq!(
            TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV,
            "HEADROOM_PROXY_TRUSTED_DASHBOARD_CLIENT_CIDRS"
        );
        // Empty -> empty list, not an error.
        assert_eq!(load_trusted_dashboard_client_cidrs("").unwrap().len(), 0);
        // Comma-separated, whitespace-tolerant.
        let nets = load_trusted_dashboard_client_cidrs("10.0.0.0/8, 192.168.1.0/24").unwrap();
        assert_eq!(nets.len(), 2);
        // A malformed entry must fail loudly and name the variable, so a typo
        // doesn't silently become an empty allow-list.
        let err = load_trusted_dashboard_client_cidrs("not-a-cidr").unwrap_err();
        assert!(
            err.starts_with("Invalid HEADROOM_PROXY_TRUSTED_DASHBOARD_CLIENT_CIDRS entry:"),
            "got {err}"
        );
    }

    #[test]
    fn dashboard_and_gateway_lists_stay_independent() {
        // Trusting a proxy to report a client IP is a different decision from
        // trusting a client to reach the Dashboard; the two env vars must not
        // be the same key.
        assert_ne!(
            TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV,
            TRUSTED_GATEWAY_CIDRS_ENV
        );
    }

    // ── same-origin ───────────────────────────────────────────────

    #[test]
    fn no_provenance_is_valid() {
        assert!(has_same_origin_or_no_provenance(
            "10.0.0.5:8787",
            None,
            None,
            "http"
        ));
    }

    #[test]
    fn matching_origin_and_referer_pass() {
        assert!(has_same_origin_or_no_provenance(
            "10.0.0.5:8787",
            Some("http://10.0.0.5:8787/"),
            Some("http://10.0.0.5:8787/stats"),
            "http"
        ));
    }

    #[test]
    fn cross_origin_fails() {
        assert!(!has_same_origin_or_no_provenance(
            "10.0.0.5:8787",
            Some("https://attacker.example/"),
            None,
            "http"
        ));
        assert!(!has_same_origin_or_no_provenance(
            "10.0.0.5:8787",
            None,
            Some("http://10.0.0.5:9999/"),
            "http"
        ));
    }

    // ── dashboard gate ────────────────────────────────────────────

    fn dashboard_headers(host: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("host", host.parse().unwrap());
        headers
    }

    #[test]
    fn loopback_views_without_grant() {
        assert!(can_view_dashboard_metadata(
            Some("127.0.0.1"),
            &dashboard_headers("127.0.0.1:8787"),
            &[],
            &[],
        ));
    }

    #[test]
    fn remote_without_grant_is_denied() {
        assert!(!can_view_dashboard_metadata(
            Some("203.0.113.7"),
            &dashboard_headers("10.0.0.5:8787"),
            &[],
            &[],
        ));
    }

    #[test]
    fn remote_with_cidr_grant_views() {
        let dashboard = load_trusted_dashboard_client_cidrs("203.0.113.0/24").unwrap();
        assert!(can_view_dashboard_metadata(
            Some("203.0.113.7"),
            &dashboard_headers("10.0.0.5:8787"),
            &[],
            &dashboard,
        ));
    }

    #[test]
    fn grant_does_not_survive_cross_origin() {
        let dashboard = load_trusted_dashboard_client_cidrs("203.0.113.0/24").unwrap();
        let mut headers = dashboard_headers("10.0.0.5:8787");
        headers.insert("origin", "https://attacker.example".parse().unwrap());
        assert!(!can_view_dashboard_metadata(
            Some("203.0.113.7"),
            &headers,
            &[],
            &dashboard,
        ));
    }

    #[test]
    fn grant_does_not_survive_hostname_host() {
        // DNS-rebinding defence: the Host must be an IP literal even with a
        // CIDR grant.
        let dashboard = load_trusted_dashboard_client_cidrs("203.0.113.0/24").unwrap();
        assert!(!can_view_dashboard_metadata(
            Some("203.0.113.7"),
            &dashboard_headers("dashboard.internal"),
            &[],
            &dashboard,
        ));
    }
}
