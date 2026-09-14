//! CTX-5 — URL fetch + HTML→markdown + disk cache + FTS indexing.
//!
//! Port of context-mode's `ctx_fetch_and_index`, tracking upstream `next`
//! (`589d821..e31360d`):
//! - P1 shell detection (`0ce043f`): refuse JS-rendered shells instead of
//!   indexing them as the page.
//! - P2 rung 1 (`5b9c00c`): `Accept: text/markdown` on the request already
//!   being made — zero extra round trips.
//! - P3 rung 2 (`8476db7`): `.md` sibling → `llms.txt` fallback, climbed only
//!   when rung 1 returned a shell.
//! - P4 block template/content split (`5b9c00c`): `fetch_blocks` +
//!   `fetch_pages`; the FTS index gets content blocks only, the page store
//!   keeps every byte.
//!
//! The raw page bytes never enter the conversation — they live in the stores
//! and the model retrieves sections via `headroom ctx search`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use headroom_core::ctx::{CtxStore, IndexOpts, SourceMeta};

use super::fetch_pages::{
    extract_and_store, route_skips_extraction, ExtractOutcome, FetchRoute, PageStore,
};

/// Default cache TTL: 24 hours.
const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum response body size: 10 MB.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// P2 rung 1: ask for the machine-readable page on the SAME request already
/// being made. The q-values keep it a superset of the old request, so no
/// site can newly break. Measured upstream 2026-08-12 (Stripe 1.8 MB HTML →
/// 11.7 kB article; GitBook, Mintlify, Resend, Polygon likewise).
pub const ACCEPT_MARKDOWN: &str =
    "text/markdown, text/x-markdown;q=0.9, text/html;q=0.8, application/xhtml+xml;q=0.8, */*;q=0.5";

/// P1: converted text below which a document carries no usable content. A
/// page title alone lands around 20 bytes; a one-line answer around 100.
pub const SHELL_MAX_TEXT_BYTES: usize = 200;
/// P1: fraction of received bytes that must survive conversion. Below this,
/// the bytes served were overwhelmingly markup and script, not text.
pub const SHELL_MAX_YIELD: f64 = 0.02;

/// Which rung of the ladder answered (reported on every fetch, never
/// inferred from byte counts).
pub mod rung {
    pub const ACCEPT_MARKDOWN: &str = "1-accept-markdown";
    pub const HTML_CONVERTED: &str = "1-html-converted";
    pub const JSON_PASSTHROUGH: &str = "1-json-passthrough";
    pub const TEXT_PASSTHROUGH: &str = "1-text-passthrough";
    pub const MD_SIBLING: &str = "2a-md-sibling";
    pub const LLMS_TXT: &str = "2b-llms-txt";
    pub const CACHE_HIT: &str = "cache-hit";
}

/// How the ladder's rung ids read in a report.
pub fn describe_rung(rung_id: &str) -> String {
    match rung_id {
        rung::ACCEPT_MARKDOWN => "rung 1 — the site served markdown to the Accept header on the request we were already making".to_string(),
        rung::HTML_CONVERTED => {
            "rung 1 — the site served HTML and it converted to an article".to_string()
        }
        rung::JSON_PASSTHROUGH => "rung 1 — JSON response, indexed as-is".to_string(),
        rung::TEXT_PASSTHROUGH => "rung 1 — plain-text response, indexed as-is".to_string(),
        rung::MD_SIBLING => "rung 2a — rung 1 returned a JavaScript shell, and the page's .md sibling carried the article".to_string(),
        rung::LLMS_TXT => "rung 2b — rung 1 returned a JavaScript shell, and this host's llms.txt named the article elsewhere".to_string(),
        // Upstream ids that this in-process ladder never emits (the shell and
        // exhausted-ladder cases return errors instead): kept so the shared
        // vocabulary stays complete if one ever flows through.
        "ladder-exhausted" => "ladder exhausted — every rung came back empty".to_string(),
        "unreported" => "rung not reported (older fetch bundle)".to_string(),
        rung::CACHE_HIT => "cache hit — served from the content index, no request made".to_string(),
        other => format!("rung {other}"),
    }
}

/// P1 shell verdict: the response was a JS-rendered shell, not the page.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    pub text_bytes: usize,
    pub source_bytes: usize,
    pub yield_pct: f64,
}

/// P1 (`0ce043f`): require BOTH signals before accusing. A ratio alone
/// condemns a small valid page (`<p>Hello</p>` is 5 bytes of text from 38 of
/// markup and is perfectly fine); a floor alone condemns a genuinely short
/// document. Zero/unknown `source_bytes` means no evidence — never accuse.
/// Arithmetic only: no pattern matching, no markup sniffing, no word lists.
pub fn classify_extraction(text_bytes: usize, source_bytes: usize) -> Option<ShellInfo> {
    if source_bytes == 0 {
        return None;
    }
    if text_bytes >= SHELL_MAX_TEXT_BYTES {
        return None;
    }
    let ratio = text_bytes as f64 / source_bytes as f64;
    if ratio >= SHELL_MAX_YIELD {
        return None;
    }
    Some(ShellInfo {
        text_bytes,
        source_bytes,
        yield_pct: ratio * 100.0,
    })
}

/// Honest refusal for a shell: carry both byte counts, name the rung-2 URLs
/// already tried so the caller does not go hunting for them, and say why a
/// retry returns the same bytes.
fn shell_refusal(url: &str, info: &ShellInfo, tried: &[String]) -> String {
    let climbed = if tried.is_empty() {
        String::new()
    } else {
        format!(
            " The ladder was climbed first and every rung came back empty: {}.",
            tried.join(", ")
        )
    };
    format!(
        "Fetched {url} but extracted only {} bytes of text from {} bytes received ({:.2}% yield) — \
         the response was a shell whose content is rendered client-side by JavaScript, so an HTTP \
         fetch cannot see it. Nothing was indexed (the response is stored whole and unaltered).\
         {climbed} Retrying this URL returns the same shell. Fetch this page's source instead: \
         its repository README or raw doc file on GitHub, its OpenAPI or JSON schema endpoint, \
         or a sibling page of the same host that is server-rendered.",
        info.text_bytes, info.source_bytes, info.yield_pct
    )
}

/// P3 rung 2a: `.md` sibling candidates for a page path, most conventional
/// first. Byte-faithful to upstream's `mdSiblingUrls`, which concatenates
/// `origin + path` (dropping any query): for the root path `/` that yields
/// `origin + ".md"` — an odd URL that fails safe (DNS error, caught and
/// recorded in `tried`), reproduced here so tried-URL reporting matches.
pub fn md_sibling_urls(page_url: &str) -> Vec<String> {
    let parsed = match url::Url::parse(page_url) {
        Ok(url) => url,
        Err(_) => return Vec::new(),
    };
    let pathname = parsed.path().to_string();
    let mut paths = Vec::new();
    if pathname.len() > ".html".len() && pathname.ends_with(".html") {
        paths.push(format!(
            "{}.md",
            &pathname[..pathname.len() - ".html".len()]
        ));
    } else if pathname.ends_with('/') {
        paths.push(format!("{}.md", &pathname[..pathname.len() - 1]));
        paths.push(format!("{pathname}index.md"));
    } else {
        paths.push(format!("{pathname}.md"));
        paths.push(format!("{pathname}/index.md"));
    }
    let origin = parsed.origin().ascii_serialization();
    let mut urls = Vec::new();
    for p in paths {
        let s = format!("{origin}{p}");
        if !urls.contains(&s) {
            urls.push(s);
        }
    }
    urls
}

/// P3: a machine-readable sibling is accepted only when the server did not
/// hand back an HTML page. The common failure is a soft 404 — status 200
/// carrying the SPA shell — caught structurally, NOT by guessing at the
/// body's shape: Apple serves its `.md` with an empty Content-Type and an
/// HTML comment as its first bytes (a `starts with #` test would reject a
/// real article), while angular.dev answers a missing `.md` with 200 + the
/// SPA shell (status alone would accept it).
pub fn is_machine_readable(status: u16, content_type: &str, body: &str) -> bool {
    if status != 200 {
        return false;
    }
    if content_type.to_lowercase().contains("html") {
        return false;
    }
    let lower = body.to_lowercase();
    if lower.contains("<!doctype html") || lower.contains("<html") {
        return false;
    }
    !body.trim().is_empty()
}

/// P3 rung 2b: `llms.txt` is an index, not the page. It is only useful when
/// it names THIS page at a location rung 2a did not already try — a site
/// publishing its markdown on another host is the case this covers. The
/// path-suffix match is segment-safe because `pathname` always begins with
/// `/`, so the match can only start at a segment boundary.
///
/// NOTE — intentional deviation from upstream: the reference splits the body
/// on the two-character sequence backslash-n instead of a real newline, so it
/// only ever examines the FIRST link of the whole file (consistent with its
/// own measurement that rung 2b adds zero value on its sample). Splitting on
/// real newlines matches the documented intent — an index with many entries —
/// so this port examines every line. Worth reporting upstream.
pub fn llms_target_for(
    body: &str,
    pathname: &str,
    already_tried: &[String],
    base: &str,
) -> Option<String> {
    if pathname.is_empty() {
        return None;
    }
    let base_url = url::Url::parse(base).ok()?;
    for line in body.split('\n') {
        let open = match line.find("](") {
            Some(i) => i,
            None => continue,
        };
        let after = &line[open + 2..];
        let close = match after.find(')') {
            Some(i) => i,
            None => continue,
        };
        let target = after[..close].trim();
        if target.is_empty() {
            continue;
        }
        let abs = base_url.join(target).ok()?.to_string();
        let entry_path = url::Url::parse(&abs).ok()?.path().to_string();
        let bare = if entry_path.len() > ".md".len() && entry_path.ends_with(".md") {
            &entry_path[..entry_path.len() - ".md".len()]
        } else {
            entry_path.as_str()
        };
        let names = bare == pathname
            || format!("{bare}/") == pathname
            || bare == format!("{pathname}/")
            || (bare.len() > pathname.len() && bare.ends_with(pathname));
        if !names {
            continue;
        }
        if abs == base {
            continue;
        }
        if already_tried.iter().any(|t| t == &abs) {
            continue;
        }
        return Some(abs);
    }
    None
}

/// Result of a fetch+index operation.
#[derive(Debug, Clone)]
pub struct FetchResult {
    pub label: String,
    pub chunks: usize,
    pub bytes: usize,
    pub cached: bool,
    pub age: Option<String>,
    /// Which rung of the ladder answered (or `cache-hit`).
    pub rung: String,
    /// One line of extraction accounting, or `None` when nothing was
    /// classified (cache hit, JSON/text passthrough).
    pub extraction: Option<String>,
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
    let parts: Vec<&str> = dt.split(['-', ' ', ':']).collect();
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
    // Normalize IPv4-mapped IPv6 (::ffff:10.x) to its v4 form so the v4
    // arms below apply — otherwise a mapped private address slips through.
    let owned;
    let ip: &IpAddr = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => {
                owned = IpAddr::V4(mapped);
                &owned
            }
            None => ip,
        },
        _ => ip,
    };
    match ip {
        IpAddr::V4(v4) => {
            // Unspecified (0.0.0.0/8) routes to localhost on typical stacks.
            // Note: `is_unspecified()` covers only 0.0.0.0 itself, so the
            // whole /8 is rejected explicitly (FINDING-023 residual).
            if v4.is_unspecified() || v4.octets()[0] == 0 {
                return Err("unspecified address not allowed".into());
            }
            // Loopback
            if v4.is_loopback() {
                return Err("loopback address not allowed".into());
            }
            // Private (RFC1918)
            if v4.is_private() {
                return Err("private address not allowed".into());
            }
            // Link-local
            if v4.is_link_local() {
                return Err("link-local address not allowed".into());
            }
            // Multicast / reserved (octet check: is_reserved/is_broadcast
            // are unstable on this toolchain; >= 224 covers multicast,
            // 240/4 reserved, and limited-broadcast 255.255.255.255).
            let octets = v4.octets();
            if v4.is_multicast() || octets[0] >= 240 || octets == [255, 255, 255, 255] {
                return Err("multicast/reserved address not allowed".into());
            }
            // Shared (100.64/10), benchmarking (198.18/15), and TEST-NET
            // documentation (192.0.2/24, 198.51.100/24, 203.0.113/24)
            // ranges are non-routable like privates.
            if (octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127)
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
            {
                return Err("non-routable address not allowed".into());
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                return Err("unspecified address not allowed".into());
            }
            if v6.is_loopback() {
                return Err("IPv6 loopback not allowed".into());
            }
            let segs = v6.segments();
            if segs[0] & 0xffc0 == 0xfe80 {
                return Err("IPv6 link-local not allowed".into());
            }
            if v6.is_multicast() {
                return Err("IPv6 multicast not allowed".into());
            }
            if segs[0] & 0xfe00 == 0xfc00 {
                return Err("IPv6 ULA not allowed".into());
            }
            // IPv4-compatible (::10.x) reaches v4 space on dual stacks.
            if segs[0..6] == [0, 0, 0, 0, 0, 0] {
                return Err("IPv4-compatible address not allowed".into());
            }
        }
    }
    Ok(())
}

/// One fetched response: status + content type + capped body.
struct FetchedResponse {
    status: reqwest::StatusCode,
    content_type: String,
    body: Vec<u8>,
}

/// Maximum redirects followed per fetch. Parity with upstream
/// `MAX_REDIRECTS`: the initial request plus up to 5 redirect hops.
const MAX_REDIRECTS: usize = 5;

/// Neutral redirect-chain-exhausted message. A benign locale or consent
/// redirect loop produces this too (measured upstream on Google devsite
/// hosts), so it must not accuse an attack.
const REDIRECT_CHAIN_EXHAUSTED: &str =
    "redirect chain exceeded 5 hops, so the walk stopped before the SSRF check \
    could be re-run on another hop. A benign locale or consent redirect loop produces \
    this too; it is not by itself evidence of an attack. Fetch the page from a host \
    that does not bounce, or fetch its raw source file directly.";

/// Resolve one redirect hop against the current URL, allowing http(s) only.
/// Pure (no I/O) so the redirect policy is unit-testable without a network.
fn resolve_redirect(current: &str, location: &str) -> Result<String, String> {
    let base = url::Url::parse(current)
        .map_err(|_| format!("SSRF blocked: invalid redirect Location: {location}"))?;
    let next = base
        .join(location)
        .map_err(|_| format!("SSRF blocked: invalid redirect Location: {location}"))?;
    match next.scheme() {
        "http" | "https" => Ok(next.to_string()),
        // Message shape mirrors upstream (`nextParsed.protocol` carries the
        // trailing colon, e.g. "file:").
        other => Err(format!(
            "SSRF blocked: redirect to non-http(s) scheme {other}:"
        )),
    }
}

/// SSRF-checked GET with the rung-1 Accept header, capped body, and a
/// MANUAL redirect walk. Upstream follows 3xx itself (`fetchWithManualRedirect`)
/// so every hop re-runs the SSRF guard: a `Location` header can otherwise
/// rebind the fetch to a host the pre-flight check never saw (a redirect
/// target that is a literal IP skips DNS resolution entirely, so even the
/// hostname pre-check would never see it). reqwest's built-in follower
/// performs no such check, so it is disabled and each hop goes through
/// [`ssrf_check`]. Shared by rung 1 and every rung-2 hop so each hop is
/// guarded, not just the first.
async fn get_url(client: &reqwest::Client, url: &str) -> Result<FetchedResponse, String> {
    let mut current = url.to_string();
    for hop in 0..=MAX_REDIRECTS {
        ssrf_check(&current).await?;
        let resp = client
            .get(&current)
            .header("User-Agent", "headroom-ctx/1.0")
            .header("Accept", ACCEPT_MARKDOWN)
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;

        let status = resp.status();
        let is_redirect = (300..400).contains(&status.as_u16());
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if is_redirect && !location.is_empty() {
            if hop == MAX_REDIRECTS {
                return Err(REDIRECT_CHAIN_EXHAUSTED.to_string());
            }
            current = resolve_redirect(&current, &location)?;
            continue;
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?
            .to_vec();

        if body.len() > MAX_BODY_BYTES {
            return Err(format!(
                "response too large: {} bytes (max {})",
                body.len(),
                MAX_BODY_BYTES
            ));
        }

        return Ok(FetchedResponse {
            status,
            content_type,
            body,
        });
    }
    unreachable!("redirect loop always returns or errors within MAX_REDIRECTS hops");
}

/// Outcome of climbing rung 2: the recovered document (empty when every
/// rung came back empty), which sub-rung answered, and every URL requested
/// so a refusal can name them.
struct Rung2Outcome {
    body: String,
    rung: String,
    tried: Vec<String>,
}

/// P3 (`8476db7`): reached ONLY when rung 1 returned a shell, so the happy
/// path still costs exactly the one request it always did. A missing sibling
/// is the expected case, not an error.
async fn climb_rung2(client: &reqwest::Client, page_url: &str) -> Rung2Outcome {
    let mut tried = Vec::new();
    for sibling in md_sibling_urls(page_url) {
        tried.push(sibling.clone());
        if let Ok(r) = get_url(client, &sibling).await {
            let text = String::from_utf8_lossy(&r.body).into_owned();
            if is_machine_readable(r.status.as_u16(), &r.content_type, &text) {
                return Rung2Outcome {
                    body: text,
                    rung: rung::MD_SIBLING.to_string(),
                    tried,
                };
            }
        }
        // A missing sibling is the expected case, not an error.
    }
    let origin = match url::Url::parse(page_url) {
        Ok(url) => url.origin().ascii_serialization(),
        Err(_) => {
            return Rung2Outcome {
                body: String::new(),
                rung: String::new(),
                tried,
            };
        }
    };
    let llms_url = format!("{origin}/llms.txt");
    tried.push(llms_url.clone());
    let llms_body = match get_url(client, &llms_url).await {
        Ok(r) => {
            let text = String::from_utf8_lossy(&r.body).into_owned();
            if is_machine_readable(r.status.as_u16(), &r.content_type, &text) {
                text
            } else {
                String::new()
            }
        }
        Err(_) => String::new(),
    };
    if !llms_body.is_empty() {
        let pathname = url::Url::parse(page_url)
            .map(|url| url.path().to_string())
            .unwrap_or_else(|_| "/".to_string());
        if let Some(target) = llms_target_for(&llms_body, &pathname, &tried, page_url) {
            tried.push(target.clone());
            if let Ok(r) = get_url(client, &target).await {
                let text = String::from_utf8_lossy(&r.body).into_owned();
                if is_machine_readable(r.status.as_u16(), &r.content_type, &text) {
                    return Rung2Outcome {
                        body: text,
                        rung: rung::LLMS_TXT.to_string(),
                        tried,
                    };
                }
            }
        }
    }
    Rung2Outcome {
        body: String::new(),
        rung: String::new(),
        tried,
    }
}

/// Some sites answer the markdown Accept with the markdown document but
/// label it `text/plain` (measured upstream: cursor.com/docs returns 16 kB
/// of `# Rules ...` as text/plain). An ATX H1 on the first non-blank line is
/// a structural check, not a threshold — and it only ever changes which
/// chunker runs, so a false positive cannot lose a byte.
fn first_line_is_atx_h1(text: &str) -> bool {
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed.starts_with("# ");
    }
    false
}

/// Pure rung-1 routing: response content-type + body text to
/// (document, route, rung id). No I/O, so the whole routing table is
/// unit-testable without a network.
///
/// Format conversion is historical and deliberately unchanged by this port
/// (JSON pretty-print, htmd for HTML *and* other text/*, plain passthrough);
/// only the extraction route is new. Upstream converts only HTML and passes
/// other text through raw — the htmd-on-text/* difference predates this port
/// and is kept to avoid changing indexed bytes for existing users.
fn route_document(content_type: &str, body_text: String) -> (String, FetchRoute, String) {
    let ct = content_type.to_lowercase();
    if ct.contains("text/markdown") || ct.contains("text/x-markdown") {
        (
            body_text,
            FetchRoute::Markdown,
            rung::ACCEPT_MARKDOWN.to_string(),
        )
    } else if ct.contains("json") {
        // Upstream assigns the text route (not JSON) when the body fails to
        // parse — both skip extraction, but the rung must be honest.
        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(v) => {
                let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| body_text.clone());
                (pretty, FetchRoute::Json, rung::JSON_PASSTHROUGH.to_string())
            }
            Err(_) => (
                body_text,
                FetchRoute::Text,
                rung::TEXT_PASSTHROUGH.to_string(),
            ),
        }
    } else if ct.contains("text/html") || ct.contains("application/xhtml") {
        let converted = htmd::convert(&body_text).unwrap_or_else(|_| body_text.clone());
        (
            converted,
            FetchRoute::Html,
            rung::HTML_CONVERTED.to_string(),
        )
    } else if ct.contains("text") {
        // Other text/* keeps its historical htmd conversion; only the route
        // is new (skip extraction, unless the body is really markdown served
        // as text/plain).
        let converted = htmd::convert(&body_text).unwrap_or_else(|_| body_text.clone());
        if first_line_is_atx_h1(&body_text) {
            (
                body_text,
                FetchRoute::Markdown,
                rung::ACCEPT_MARKDOWN.to_string(),
            )
        } else {
            (
                converted,
                FetchRoute::Text,
                rung::TEXT_PASSTHROUGH.to_string(),
            )
        }
    } else {
        (
            body_text,
            FetchRoute::Text,
            rung::TEXT_PASSTHROUGH.to_string(),
        )
    }
}

/// The page sidecar for this content DB: `fetch-pages.db` beside it (as
/// upstream keeps it beside the FTS content DB). In-memory when the content
/// path has no parent dir (tests). `None` only when even the in-memory
/// fallback fails — the caller then indexes the full document and says so.
fn page_store_for(content_path: &std::path::Path) -> Option<PageStore> {
    match content_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => PageStore::open(&dir.join("fetch-pages.db"))
            .or_else(|_| PageStore::open_in_memory())
            .ok(),
        _ => PageStore::open_in_memory().ok(),
    }
}

/// Fetch a URL, convert HTML→markdown, and index into the FTS store.
/// Checks disk cache first; skips fetch if content is fresh within TTL.
///
/// Ladder, cheapest correct answer first: (1) the site's own markdown via
/// the Accept header, else converted HTML; (2) `.md` sibling / `llms.txt`
/// when rung 1 returned a shell; (4) block classification against other
/// pages of the host; (5) an honest refusal naming the URLs already tried.
/// Rungs past 1 cost a request only when the cheaper rung returned a shell.
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
                    rung: rung::CACHE_HIT.to_string(),
                    extraction: None,
                });
            }
        }
    }

    // Fetch the URL (rung 1). Redirects are followed manually inside
    // `get_url` (never by reqwest) so every hop is SSRF-checked.
    let client = crate::ssl_context::client_builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let rung1 = get_url(&client, url).await?;

    if !rung1.status.is_success() {
        return Err(format!("HTTP {}", rung1.status));
    }

    let source_bytes = rung1.body.len();
    let body_text = String::from_utf8_lossy(&rung1.body).into_owned();

    let (mut document, route, mut rung_id) = route_document(&rung1.content_type, body_text);

    // P1 + P3: a shell means climb rung 2 before giving up. An empty HTML
    // conversion counts as a shell here (0 bytes at 0% yield), exactly as
    // upstream's subprocess verdict does — but if the ladder then comes back
    // empty, the parent reports "empty content", not a shell refusal.
    if route == FetchRoute::Html {
        let text_len = document.trim().len();
        if let Some(shell) = classify_extraction(text_len, source_bytes) {
            let climbed = climb_rung2(&client, url).await;
            if climbed.body.is_empty() {
                if document.trim().is_empty() {
                    return Err("empty content after conversion".into());
                }
                return Err(shell_refusal(url, &shell, &climbed.tried));
            }
            // Recovered site-authored markdown still gets the shell check for
            // uniformity (a real article passes: its yield is ~100%).
            let recovered_len = climbed.body.trim().len();
            if let Some(shell) = classify_extraction(recovered_len, climbed.body.len()) {
                return Err(shell_refusal(url, &shell, &climbed.tried));
            }
            document = climbed.body;
            rung_id = climbed.rung;
        }
    }
    let route = if rung_id == rung::MD_SIBLING || rung_id == rung::LLMS_TXT {
        FetchRoute::Markdown
    } else {
        route
    };

    // Upstream's parent reports "empty content" for an empty document on any
    // route (the check runs after the ladder, so an empty HTML conversion
    // that exhausted rung 2 lands here too).
    if document.trim().is_empty() {
        return Err("empty content after conversion".into());
    }

    // P4: template/content split. JSON and plain text are not web pages and
    // have no chrome — indexed whole, exactly as before.
    let mut index_text = document.clone();
    let mut extraction: Option<String> = None;
    if !route_skips_extraction(route) {
        let outcome = match page_store_for(store.path()) {
            Some(ps) => extract_and_store(url, &label, &document, route, &ps)
                .map_err(|e| format!("extraction unavailable ({e})")),
            None => Err("extraction unavailable (page store would not open)".to_string()),
        };
        match outcome {
            Ok(ExtractOutcome::Index {
                index_text: classified,
                stored_bytes,
                content_bytes,
                template_bytes,
                template_blocks,
                total_blocks,
                provisional,
                relabelled,
                ..
            }) => {
                // A second page of a host resolves the cold-start labelling
                // of every page before it: re-index those under their own
                // labels (a failure leaves the earlier, larger index in
                // place — never an empty one). The note below counts every
                // store-relabeled page, as upstream does — the relabelling
                // itself succeeded even if a re-index write failed.
                for prev in &relabelled {
                    let opts = IndexOpts {
                        plain_text_lines: Some(50),
                        ..Default::default()
                    };
                    let _ = store.index_content(&prev.source_label, &prev.index_text, &opts);
                }
                let rung_desc = describe_rung(&rung_id);
                let mut note = if route == FetchRoute::Markdown {
                    format!(
                        "{rung_desc}; site-authored markdown ({stored_bytes} B) — no extraction needed"
                    )
                } else if provisional {
                    format!(
                        "{rung_desc}; rung 4 (block classification) — first page seen from this host, so all \
                         {total_blocks} blocks were indexed as content (PROVISIONAL); they are re-classified \
                         automatically when a second page of this host is fetched"
                    )
                } else {
                    format!(
                        "{rung_desc}; rung 4 (block classification) — {}/{} blocks indexed as content \
                         ({} B); {} blocks ({} B) were seen on other pages of this host, so they are \
                         labelled template — stored whole, kept out of the index",
                        total_blocks - template_blocks,
                        total_blocks,
                        content_bytes,
                        template_blocks,
                        template_bytes
                    )
                };
                if !relabelled.is_empty() {
                    note.push_str(&format!(
                        "; re-classified {} earlier page(s) of this host now that a second page exists",
                        relabelled.len()
                    ));
                }
                extraction = Some(note);
                index_text = classified;
            }
            Ok(ExtractOutcome::Refuse { reason, .. }) => {
                // Rung 5 — the honest refusal, with the rung that produced
                // the document named so the reader sees how far the ladder
                // got. Reporting a shell as success would hand the caller a
                // site shell dressed as an article; on an error the model
                // tries another route, on a false success it stops looking.
                return Err(format!("{}; {reason}", describe_rung(&rung_id)));
            }
            Err(e) => {
                // Extraction is an optimisation on top of a working fetch. If
                // the block store cannot be opened, index the whole document
                // exactly as before rather than losing the page.
                extraction = Some(format!("{e} — indexed the full document"));
            }
        }
    }

    let bytes = document.len();

    // Index into FTS store. NOTE (parity gap, pre-existing): upstream indexes
    // per content header — JSON by key paths, markdown/HTML with heading-aware
    // markdown chunking, plain text directly — while this port keeps the
    // historical plain-text-50 chunking for every route. The extraction pass
    // above already decides *which* bytes reach the index; the chunker decides
    // how they are windowed. Changing it is a separate behaviour change.
    let opts = IndexOpts {
        plain_text_lines: Some(50),
        ..Default::default()
    };
    let summary = store
        .index_content(&label, &index_text, &opts)
        .map_err(|e| format!("index failed: {e}"))?;

    Ok(FetchResult {
        label: summary.label,
        chunks: summary.total_chunks,
        bytes,
        cached: false,
        age: None,
        rung: rung_id,
        extraction,
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
    fn ssrf_guard_rejects_localhost_reaching_families() {
        // FINDING-023: 0.0.0.0/8 routes to localhost on typical stacks.
        let ip: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "0.1.2.3".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        // FINDING-023: IPv4-mapped IPv6 carrying private/loopback v4.
        let ip: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(check_ip(&ip).is_ok());
        // Non-routable v4 ranges (shared/benchmark/TED-3 documentation).
        let ip: IpAddr = "100.64.0.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert!(check_ip(&ip).is_err());
        let ip: IpAddr = "::".parse().unwrap();
        assert!(check_ip(&ip).is_err());
    }

    #[test]
    fn format_age_variants() {
        assert_eq!(format_age(Duration::from_secs(30)), "just now");
        assert_eq!(format_age(Duration::from_secs(120)), "2m ago");
        assert_eq!(format_age(Duration::from_secs(7200)), "2h ago");
        assert_eq!(format_age(Duration::from_secs(172800)), "2d ago");
    }

    /// P1 vectors, measured upstream on this exact code path (bytes received
    /// -> markdown bytes out). Only the first row is a shell.
    #[test]
    fn shell_detection_matches_upstream_measurements() {
        // excalidraw.com: 6,862 -> 21 (0.31% yield) — the shell.
        assert!(classify_extraction(21, 6862).is_some());
        // app.diagrams.net, nextjs.org, cloudflare dev docs: real pages.
        assert!(classify_extraction(476, 2759).is_none());
        assert!(classify_extraction(13587, 316151).is_none());
        assert!(classify_extraction(8389, 178627).is_none());
        // A ratio alone must not condemn a small valid page.
        assert!(classify_extraction(5, 38).is_none());
        // A floor alone must not condemn a genuinely short document.
        assert!(classify_extraction(150, 200).is_none());
        // No evidence (older bundle never reported source bytes): never accuse.
        assert!(classify_extraction(21, 0).is_none());
        // Boundary: 200 bytes of text is content, 199 at low yield is shell.
        assert!(classify_extraction(200, 100_000).is_none());
        assert!(classify_extraction(199, 100_000).is_some());
    }

    #[test]
    fn shell_refusal_names_tried_urls() {
        let info = classify_extraction(21, 6862).expect("shell");
        let err = shell_refusal(
            "https://x.dev/a",
            &info,
            &["https://x.dev/a.md".to_string()],
        );
        assert!(err.contains("21 bytes of text from 6862 bytes received"));
        assert!(err.contains("0.31% yield"));
        assert!(err.contains("https://x.dev/a.md"));
        assert!(err.contains("Retrying this URL returns the same shell"));
        let bare = shell_refusal("https://x.dev/a", &info, &[]);
        assert!(!bare.contains("ladder was climbed"));
    }

    #[test]
    fn md_sibling_candidates_follow_convention_order() {
        assert_eq!(
            md_sibling_urls("https://x.dev/docs/view"),
            vec![
                "https://x.dev/docs/view.md".to_string(),
                "https://x.dev/docs/view/index.md".to_string(),
            ]
        );
        assert_eq!(
            md_sibling_urls("https://x.dev/docs/page.html"),
            vec!["https://x.dev/docs/page.md".to_string()]
        );
        assert_eq!(
            md_sibling_urls("https://x.dev/docs/"),
            vec![
                "https://x.dev/docs.md".to_string(),
                "https://x.dev/docs/index.md".to_string(),
            ]
        );
        // Byte-faithful to upstream: the root path concatenates origin + ".md".
        // Odd, fails safe (DNS error, caught), and keeps tried-URL reporting
        // identical to the reference implementation.
        assert_eq!(
            md_sibling_urls("https://x.dev/"),
            vec![
                "https://x.dev.md".to_string(),
                "https://x.dev/index.md".to_string(),
            ]
        );
        assert!(md_sibling_urls("::not-a-url::").is_empty());
    }

    #[test]
    fn machine_readable_accepts_apple_rejects_shell() {
        // Apple serves its .md with an EMPTY content type and an HTML comment
        // first — accepted (structural check, not starts-with-#).
        assert!(is_machine_readable(
            200,
            "",
            "<!-- apple -->\n# View\nReal article.\n"
        ));
        // Soft 404: 200 carrying the SPA shell.
        assert!(!is_machine_readable(
            200,
            "text/html",
            "<!doctype html><html><body><div id=root></div></body></html>"
        ));
        assert!(!is_machine_readable(
            200,
            "text/markdown",
            "<html><body>shell</body></html>"
        ));
        assert!(!is_machine_readable(404, "text/markdown", "# Real\n"));
        assert!(!is_machine_readable(200, "text/markdown", "   \n  "));
    }

    #[test]
    fn llms_txt_names_this_page_elsewhere() {
        let body = "# Index\n\n- [View](https://x.dev/docs/view.md)\n- [Other](https://x.dev/docs/other.md)\n";
        assert_eq!(
            llms_target_for(body, "/docs/view", &[], "https://x.dev/docs/view"),
            Some("https://x.dev/docs/view.md".to_string())
        );
        // Already tried by rung 2a: no second use.
        assert_eq!(
            llms_target_for(
                body,
                "/docs/view",
                &["https://x.dev/docs/view.md".to_string()],
                "https://x.dev/docs/view"
            ),
            None
        );
        // Entries for other pages do not match.
        assert_eq!(
            llms_target_for(body, "/docs/missing", &[], "https://x.dev/docs/missing"),
            None
        );
        // Multi-entry indexes are scanned line by line: a match on a later
        // line is found (the reference only sees the first link — see the
        // deviation note on `llms_target_for`).
        let multi = "# Index\n\n- [Other](https://x.dev/docs/other.md)\n- [View](./view.md)\n";
        assert_eq!(
            llms_target_for(multi, "/docs/view", &[], "https://x.dev/docs/view"),
            Some("https://x.dev/docs/view.md".to_string())
        );
        // The index itself is never the answer.
        assert_eq!(
            llms_target_for(
                "- [Home](https://x.dev/docs/view)\n",
                "/docs/view",
                &[],
                "https://x.dev/docs/view"
            ),
            None
        );
    }

    #[test]
    fn atx_h1_detects_mislabeled_markdown() {
        assert!(first_line_is_atx_h1("\n  \n# Rules\nBody\n"));
        assert!(!first_line_is_atx_h1("Just text\n# Late heading\n"));
        assert!(!first_line_is_atx_h1("   \n  "));
    }

    #[test]
    fn routing_table_matches_upstream_branches() {
        // Site-authored markdown, case-insensitive content type.
        let (doc, route, rung) =
            route_document("Text/Markdown; charset=utf-8", "# T\n".to_string());
        assert_eq!(doc, "# T\n");
        assert_eq!(route, FetchRoute::Markdown);
        assert_eq!(rung, rung::ACCEPT_MARKDOWN);
        // JSON pretty-prints; unparseable JSON falls to the text route.
        let (doc, route, rung) = route_document("application/json", r#"{"a":1}"#.to_string());
        assert_eq!(route, FetchRoute::Json);
        assert_eq!(rung, rung::JSON_PASSTHROUGH);
        assert!(doc.contains("\"a\": 1"));
        let (doc, route, rung) = route_document("application/ld+json", "not json{".to_string());
        assert_eq!(doc, "not json{");
        assert_eq!(route, FetchRoute::Text);
        assert_eq!(rung, rung::TEXT_PASSTHROUGH);
        // HTML converts (route Html); other text keeps historical conversion
        // but skips extraction...
        let (_, route, rung) = route_document("text/html", "<p>Hi</p>".to_string());
        assert_eq!(route, FetchRoute::Html);
        assert_eq!(rung, rung::HTML_CONVERTED);
        let (_, route, rung) = route_document("text/csv", "a,b\nc,d\n".to_string());
        assert_eq!(route, FetchRoute::Text);
        assert_eq!(rung, rung::TEXT_PASSTHROUGH);
        // ...unless the text/plain body is really markdown.
        let (doc, route, rung) = route_document("text/plain", "\n# Rules\nBody\n".to_string());
        assert_eq!(doc, "\n# Rules\nBody\n");
        assert_eq!(route, FetchRoute::Markdown);
        assert_eq!(rung, rung::ACCEPT_MARKDOWN);
        // Unknown types pass through as text.
        let (_, route, _) = route_document("application/octet-stream", "bytes".to_string());
        assert_eq!(route, FetchRoute::Text);
        // Empty content-type: no evidence either way, treated as text.
        let (_, route, _) = route_document("", "x".to_string());
        assert_eq!(route, FetchRoute::Text);
    }

    #[test]
    fn rung_descriptions_name_the_paying_step() {
        assert!(describe_rung(rung::ACCEPT_MARKDOWN).contains("rung 1"));
        assert!(describe_rung(rung::MD_SIBLING).contains("2a"));
        assert!(describe_rung(rung::CACHE_HIT).contains("cache hit"));
        // Shared vocabulary with upstream, kept even though this in-process
        // ladder never emits them (shells surface as errors instead).
        assert!(describe_rung("ladder-exhausted").contains("every rung came back empty"));
        assert!(describe_rung("unreported").contains("not reported"));
        assert_eq!(describe_rung("weird"), "rung weird");
    }

    #[test]
    fn redirect_walk_resolves_and_confines_scheme() {
        // Relative and absolute http(s) targets resolve against the current URL.
        assert_eq!(
            resolve_redirect("https://x.dev/a/b", "../c"),
            Ok("https://x.dev/c".to_string())
        );
        assert_eq!(
            resolve_redirect("https://x.dev/a", "https://y.dev/b?q=1"),
            Ok("https://y.dev/b?q=1".to_string())
        );
        // Non-http(s) targets are blocked, not followed.
        assert!(resolve_redirect("https://x.dev/a", "file:///etc/passwd").is_err());
        assert!(resolve_redirect("https://x.dev/a", "javascript:alert(1)").is_err());
        assert!(resolve_redirect("https://x.dev/a", "gopher://x.dev/1").is_err());
        // Garbage locations are blocked, not followed.
        assert!(resolve_redirect("https://x.dev/a", "http://[::1").is_err());
    }

    #[test]
    fn page_store_falls_back_to_memory_without_parent() {
        let store = page_store_for(std::path::Path::new(":memory:"));
        assert!(store.is_some());
        let store = store.expect("in-memory fallback");
        assert_eq!(
            store
                .host_page_count("example.com", "")
                .expect("count works"),
            0
        );
    }
}
