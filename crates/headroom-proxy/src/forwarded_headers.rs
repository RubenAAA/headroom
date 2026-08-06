//! Trusted-gateway gate for `X-Forwarded-*` headers.
//!
//! Pure validation and parsing functions for CIDR allow-lists and IP
//! normalization. The request-dependent `resolve_client_ip` and
//! `trusted_forwarded_headers` stay in Python.

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
}
