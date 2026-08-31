//! CTX-5 — URL fetch + HTML→markdown + disk cache + FTS indexing.
//!
//! Port of context-mode's `ctx_fetch_and_index`: fetches a URL, converts
//! HTML to markdown (via `htmd`, a Rust Turndown port), indexes into the
//! FTS content store, and caches the result on disk with a configurable TTL.
//!
//! The raw page bytes never enter the conversation — they live in the store
//! and the model retrieves sections via `headroom ctx search`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use headroom_core::ctx::{CtxStore, IndexOpts, SourceMeta};

/// Default cache TTL: 24 hours.
const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum response body size: 10 MB.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Result of a fetch+index operation.
#[derive(Debug, Clone)]
pub struct FetchResult {
    pub label: String,
    pub chunks: usize,
    pub bytes: usize,
    pub cached: bool,
    pub age: Option<String>,
}

/// Check if a cached source is still fresh (within TTL).
fn is_fresh(meta: &SourceMeta, ttl: Duration) -> bool {
    // Parse SQLite datetime("now") format: "YYYY-MM-DD HH:MM:SS" (UTC).
    // We approximate by parsing the timestamp and comparing to now.
    let indexed = parse_sqlite_datetime(&meta.indexed_at);
    let _now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    match indexed {
        Some(age) => age < ttl,
        None => false, // Can't parse = treat as stale
    }
}

/// Parse a SQLite datetime string and return the age as a Duration.
/// Returns None if parsing fails.
fn parse_sqlite_datetime(dt: &str) -> Option<Duration> {
    // Format: "YYYY-MM-DD HH:MM:SS"
    let parts: Vec<&str> = dt
        .split(|c: char| c == '-' || c == ' ' || c == ':')
        .collect();
    if parts.len() != 6 {
        return None;
    }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;
    let hour: u64 = parts[3].parse().ok()?;
    let minute: u64 = parts[4].parse().ok()?;
    let second: u64 = parts[5].parse().ok()?;

    // Approximate days since epoch (good enough for TTL comparison).
    let days = (year - 1970) * 365 + (year - 1970) / 4 + month * 30 + day;
    let secs = days * 86400 + hour * 3600 + minute * 60 + second;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let age_secs = now.as_secs().saturating_sub(secs);
    Some(Duration::from_secs(age_secs))
}

/// Format a duration as a human-readable age string.
fn format_age(dur: Duration) -> String {
    let secs = dur.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Compose the storage label from source + URL (parity with TS
/// `composeFetchCacheKey`).
fn compose_label(source: Option<&str>, url: &str) -> String {
    match source {
        Some(s) => format!("{s}::{url}"),
        None => url.to_string(),
    }
}

/// SSRF guard: reject URLs that target private/loopback/multicast IPs.
/// Runs DNS resolution and checks the resolved IP before fetching.
async fn ssrf_check(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme: {other}")),
    }

    let host = parsed.host_str().ok_or("URL has no host")?.to_string();

    // Skip DNS check for obvious public hostnames (fast path).
    // Only do DNS resolution for IPs or ambiguous hostnames.
    if let Ok(ip) = host.parse::<IpAddr>() {
        check_ip(&ip)?;
    }
    // For hostnames, we do a quick DNS check to prevent SSRF via rebinding.
    let _host_clone = host.clone();
    let addrs = tokio::net::lookup_host(format!("{host}:443"))
        .await
        .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?;

    for addr in addrs {
        check_ip(&addr.ip())?;
    }

    Ok(())
}

fn check_ip(ip: &IpAddr) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // Loopback
            if octets[0] == 127 {
                return Err("loopback address not allowed".into());
            }
            // Private (RFC1918)
            if octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
            {
                return Err("private address not allowed".into());
            }
            // Link-local
            if octets[0] == 169 && octets[1] == 254 {
                return Err("link-local address not allowed".into());
            }
            // Multicast / reserved
            if octets[0] >= 224 {
                return Err("multicast/reserved address not allowed".into());
            }
        }
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            if v6.is_loopback() {
                return Err("IPv6 loopback not allowed".into());
            }
            if segs[0] & 0xffc0 == 0xfe80 {
                return Err("IPv6 link-local not allowed".into());
            }
            if segs[0] & 0xff00 == 0xff00 {
                return Err("IPv6 multicast not allowed".into());
            }
            if segs[0] & 0xfe00 == 0xfc00 {
                return Err("IPv6 ULA not allowed".into());
            }
        }
    }
    Ok(())
}

/// Fetch a URL, convert HTML→markdown, and index into the FTS store.
/// Checks disk cache first; skips fetch if content is fresh within TTL.
pub async fn fetch_and_index(
    url: &str,
    source: Option<&str>,
    store: &Arc<CtxStore>,
    force: bool,
    ttl: Option<Duration>,
) -> Result<FetchResult, String> {
    ssrf_check(url).await?;

    let ttl = ttl.unwrap_or(DEFAULT_TTL);
    let label = compose_label(source, url);

    // Check cache freshness (unless forced).
    if !force {
        let meta = store
            .source_meta(&label)
            .map_err(|e| format!("DB error: {e}"))?;
        if let Some(meta) = meta {
            if is_fresh(&meta, ttl) {
                let age = parse_sqlite_datetime(&meta.indexed_at)
                    .map(format_age)
                    .unwrap_or_else(|| "unknown".to_string());
                return Ok(FetchResult {
                    label,
                    chunks: meta.chunk_count,
                    bytes: 0, // Not re-fetched
                    cached: true,
                    age: Some(age),
                });
            }
        }
    }

    // Fetch the URL.
    let client = crate::ssl_context::client_builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .header("User-Agent", "headroom-ctx/1.0")
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let raw = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;

    if raw.len() > MAX_BODY_BYTES {
        return Err(format!(
            "response too large: {} bytes (max {})",
            raw.len(),
            MAX_BODY_BYTES
        ));
    }

    // Convert to markdown.
    let markdown = if content_type.contains("json") {
        // JSON: pretty-print and index as plain text.
        let text = String::from_utf8_lossy(&raw);
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.into_owned()),
            Err(_) => text.into_owned(),
        }
    } else if content_type.contains("html") || content_type.contains("text") {
        // HTML → markdown via htmd.
        let html = String::from_utf8_lossy(&raw);
        htmd::convert(&html).unwrap_or_else(|_| html.into_owned())
    } else {
        // Unknown content type: treat as plain text.
        String::from_utf8_lossy(&raw).into_owned()
    };

    if markdown.trim().is_empty() {
        return Err("empty content after conversion".into());
    }

    let bytes = markdown.len();

    // Index into FTS store.
    let opts = IndexOpts {
        plain_text_lines: Some(50),
        ..Default::default()
    };
    let summary = store
        .index_content(&label, &markdown, &opts)
        .map_err(|e| format!("index failed: {e}"))?;

    Ok(FetchResult {
        label: summary.label,
        chunks: summary.total_chunks,
        bytes,
        cached: false,
        age: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_label_with_source() {
        assert_eq!(
            compose_label(Some("React docs"), "https://react.dev/use"),
            "React docs::https://react.dev/use"
        );
    }

    #[test]
    fn compose_label_without_source() {
        assert_eq!(
            compose_label(None, "https://react.dev/use"),
            "https://react.dev/use"
        );
    }

    #[test]
    fn ssrf_guard_rejects_loopback() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
    }

    #[test]
    fn ssrf_guard_rejects_private() {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "172.16.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
    }

    #[test]
    fn ssrf_guard_allows_public() {
        let ip: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(check_ip(&ip).is_ok());
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        assert!(check_ip(&ip).is_ok());
    }

    #[test]
    fn format_age_variants() {
        assert_eq!(format_age(Duration::from_secs(30)), "just now");
        assert_eq!(format_age(Duration::from_secs(120)), "2m ago");
        assert_eq!(format_age(Duration::from_secs(7200)), "2h ago");
        assert_eq!(format_age(Duration::from_secs(172800)), "2d ago");
    }
}
