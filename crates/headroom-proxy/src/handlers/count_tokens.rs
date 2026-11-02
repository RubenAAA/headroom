//! Local token counting for translated routes (`POST /v1/messages/count_tokens`).
//!
//! Claude Code calls this to estimate context. A routed alias
//! (`claude-muse-spark-1.3`) forwarded verbatim is unknown to Anthropic,
//! which answers 404 with a non-JSON page — and that refusal also pollutes
//! the upstream-health window the statusline watches. Answer translated
//! routes locally with the calibrated estimator instead.
//!
//! Anything else falls through to the standard forwarder byte-identical, so
//! counts for real Anthropic models stay exact and behavior there cannot
//! regress: this endpoint only changes responses that are 404s today.

use std::borrow::Cow;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;

use crate::proxy::{AppState, forward_http};
use crate::routed::routing::find_route_target;

/// Estimate Anthropic input tokens for a `count_tokens` body: system text,
/// message content, and tool definitions, through the model family's
/// calibrated counter (`claude-*` → 3.5 chars/token estimator, tiktoken
/// where a real tokenizer exists).
///
/// Deliberately an estimate, and a slight undercount at that: per-message
/// framing is not counted. Image/document payloads ARE weighted (see
/// `image_block_tokens`, and `push_document` for what `documents` selects):
/// without that the estimate stays flat as screenshots
/// accumulate, the client never compacts, and the turn dies at the upstream
/// byte cap. The alternative for these models today is a 404, so an honest
/// approximation strictly wins.
pub(crate) fn estimate_input_tokens(
    parsed: &Value,
    model: &str,
    documents: DocumentPricing,
) -> u64 {
    let counter = headroom_core::tokenizer::get_tokenizer(model);
    let mut text = String::new();
    let mut image_tokens: u64 = 0;
    push_content_text(&mut text, &mut image_tokens, &parsed["system"], documents);
    if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
        for message in messages {
            push_content_text(&mut text, &mut image_tokens, &message["content"], documents);
        }
    }
    // Tool definitions ride every request as input tokens.
    if parsed.get("tools").is_some() {
        text.push_str(&serde_json::to_string(&parsed["tools"]).unwrap_or_default());
    }
    counter.count_text(&text) as u64 + image_tokens
}

/// What a `document` block costs, decided by the route that receives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentPricing {
    /// The block reaches an Anthropic-protocol upstream, which bills a PDF
    /// per page.
    Billed,
    /// The route never sends the document: the OpenAI translator
    /// (`openai/request.rs`) swaps it for [`DOCUMENT_PLACEHOLDER`] and the
    /// cursor bridge keeps only text. Pricing pages there would compact the
    /// client early for tokens no model reads.
    Placeholder,
}

/// The line `openai/request.rs` sends in place of a document block.
const DOCUMENT_PLACEHOLDER: &str = "[unsupported content block omitted: document]";

/// Append the countable text of an Anthropic `content` value: a plain string,
/// or an array of blocks. Image and PDF blocks carry no text (pixels and
/// pages are billed by size, which a text counter cannot see) — their weight
/// goes to `image_tokens` instead of being skipped.
fn push_content_text(
    out: &mut String,
    image_tokens: &mut u64,
    content: &Value,
    documents: DocumentPricing,
) {
    match content {
        Value::String(s) => out.push_str(s),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(s) = block.get("text").and_then(|t| t.as_str()) {
                            out.push_str(s);
                        }
                    }
                    Some("tool_use") => {
                        // The JSON the model must reproduce counts.
                        out.push_str(&serde_json::to_string(&block["input"]).unwrap_or_default());
                    }
                    Some("tool_result") => {
                        push_content_text(out, image_tokens, &block["content"], documents)
                    }
                    Some("image") => *image_tokens += image_block_tokens(block),
                    Some("document") => match documents {
                        DocumentPricing::Billed => push_document(out, image_tokens, block),
                        DocumentPricing::Placeholder => out.push_str(DOCUMENT_PLACEHOLDER),
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Token weight of an image block, which carries no countable text.
///
/// Anthropic bills images by dimensions ((w*h)/750), not bytes, so base64
/// length alone cannot size them: read just the image header for dimensions
/// (no pixel decode) and apply the same formula as
/// `tile_optimizer::estimate_anthropic_tokens`. When dimensions are
/// unreadable (unsupported format, URL source, corrupt data), fall back to
/// decoded bytes/750 — roughly one byte per pixel.
fn image_block_tokens(block: &Value) -> u64 {
    let source = match block.get("source") {
        Some(s) => s,
        None => return 0,
    };
    // URL sources live upstream; nothing local to weigh.
    if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
        return 0;
    }
    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
    // Tolerate a data: URL prefix though Anthropic sends raw base64.
    let b64 = data.rsplit(',').next().unwrap_or(data);
    let decoded_len = b64.len() as u64 * 3 / 4;
    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(b) => b,
        Err(_) => return decoded_len / 750,
    };
    match image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
    {
        Some((w, h)) => crate::tile_optimizer::estimate_anthropic_tokens(w, h) as u64,
        None => bytes.len() as u64 / 750,
    }
}

/// Anthropic bills 1,500-3,000 tokens per PDF page; upstream (`fc1ba891b`)
/// takes the low end.
const DOCUMENT_TOKENS_PER_PAGE: u64 = 1500;
/// File bytes per page when the page count is unreadable (upstream's figure).
const DOCUMENT_BYTES_PER_PAGE: usize = 3000;
/// A larger `/Count` is malformed, not priced (upstream's bound).
const PDF_MAX_PAGES: u64 = 10_000;
/// Anthropic's request cap: a bigger file is never scanned, only sized.
const PDF_SCAN_MAX_BYTES: usize = 32 << 20;
/// Bytes walked around dictionaries, searched for stream ends, or inflated
/// from object streams, per file (upstream's 4 MB). Spent means the size
/// estimate, never a partial count.
const PDF_WORK_BUDGET: usize = 4_000_000;
/// How far a dictionary may reach from the key that names its type.
const PDF_DICT_WINDOW: usize = 64 << 10;

/// A `document` block as Anthropic bills it (upstream `_count_document_block`):
/// a base64 PDF by its pages, a text or content source as its text, a url or
/// file source as one nominal page (the bytes live with the provider), plus
/// any `title` and `context`.
fn push_document(out: &mut String, image_tokens: &mut u64, block: &Value) {
    let source = &block["source"];
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => *image_tokens += base64_document_tokens(source),
        Some("content") => push_content_text(
            out,
            image_tokens,
            &source["content"],
            DocumentPricing::Billed,
        ),
        Some("url" | "file") => *image_tokens += DOCUMENT_TOKENS_PER_PAGE,
        _ => out.push_str(source.get("data").and_then(Value::as_str).unwrap_or("")),
    }
    for key in ["title", "context"] {
        if let Some(s) = block.get(key).and_then(Value::as_str) {
            out.push_str(s);
        }
    }
}

/// A base64 document source. Only a PDF gets page pricing; any other media
/// type keeps the decoded bytes/750 weight it had before.
fn base64_document_tokens(source: &Value) -> u64 {
    let data = source.get("data").and_then(Value::as_str).unwrap_or("");
    let b64 = data.rsplit(',').next().unwrap_or(data);
    let pdf = source.get("media_type").and_then(Value::as_str) == Some("application/pdf");
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(bytes) if pdf => pdf_tokens(&bytes),
        Ok(bytes) => bytes.len() as u64 / 750,
        // Undecodable: one page, as upstream prices empty bytes.
        Err(_) if pdf => DOCUMENT_TOKENS_PER_PAGE,
        Err(_) => b64.len() as u64 * 3 / 4 / 750,
    }
}

/// Pages times the per-page rate. Without a `%PDF-` header or a readable
/// page count, one page per 3 KB of file stands in.
fn pdf_tokens(doc: &[u8]) -> u64 {
    let by_size = (doc.len() / DOCUMENT_BYTES_PER_PAGE) as u64;
    let is_pdf = memchr::memmem::find(&doc[..doc.len().min(1024)], b"%PDF-").is_some();
    let pages = if is_pdf {
        pdf_page_count(doc).unwrap_or(by_size)
    } else {
        by_size
    };
    pages.max(1) * DOCUMENT_TOKENS_PER_PAGE
}

/// The page count a PDF declares, found by scanning rather than parsing.
///
/// Upstream walks the xref chain to the active catalog. This scan skips the
/// xref: it reads every page-tree node in the file and in its Flate object
/// streams (where PDF 1.5+ writers such as LaTeX and Word put the tree), and
/// takes the `/Count` of the last root (no `/Parent`), since an incremental
/// update appends the newer tree. With no root, it counts `/Type /Page`
/// leaves. Trade-off: an object an update orphaned can still be seen, and a
/// `<<` inside a string can misplace a dictionary's edges; either gives a
/// wrong count only on unusual files. `None` sends the caller to the size
/// estimate.
fn pdf_page_count(doc: &[u8]) -> Option<u64> {
    if doc.len() > PDF_SCAN_MAX_BYTES {
        return None;
    }
    let mut scan = PdfScan {
        budget: PDF_WORK_BUDGET,
    };
    // (where in the file, bytes): an object stream's tree sits at the stream.
    let mut sources: Vec<(Option<usize>, Cow<[u8]>)> = vec![(None, Cow::Borrowed(doc))];
    for (at, inflated) in scan.object_streams(doc)? {
        sources.push((Some(at), Cow::Owned(inflated)));
    }
    let mut root: Option<(usize, Option<u64>)> = None;
    for (origin, buf) in &sources {
        for at in pdf_type_positions(buf, b"/Pages") {
            let Some((start, end)) = scan.dict_around(buf, at) else {
                if scan.budget == 0 {
                    return None;
                }
                continue;
            };
            let dict = &buf[start..end];
            if pdf_has_key(dict, b"/Parent") {
                continue;
            }
            let place = origin.unwrap_or(at);
            if root.is_none_or(|(p, _)| place >= p) {
                root = Some((place, pdf_count(dict)));
            }
        }
    }
    if let Some((_, count)) = root {
        return count;
    }
    let leaves: usize = sources
        .iter()
        .map(|(_, buf)| pdf_type_positions(buf, b"/Page").count())
        .sum();
    let leaves = leaves as u64;
    (1..=PDF_MAX_PAGES).contains(&leaves).then_some(leaves)
}

/// Work left for one file's scan; see [`PDF_WORK_BUDGET`].
struct PdfScan {
    budget: usize,
}

impl PdfScan {
    fn charge(&mut self, bytes: usize) {
        self.budget = self.budget.saturating_sub(bytes);
    }

    /// The `<< … >>` dictionary holding byte `at`, as a range, within
    /// [`PDF_DICT_WINDOW`] either side. `None` when unbalanced or spent.
    fn dict_around(&mut self, buf: &[u8], at: usize) -> Option<(usize, usize)> {
        if self.budget == 0 {
            return None;
        }
        let lo = at.saturating_sub(PDF_DICT_WINDOW);
        let mut depth = 0usize;
        let mut i = at;
        let start = loop {
            if i < lo + 2 {
                break None;
            }
            match &buf[i - 2..i] {
                b"<<" if depth == 0 => break Some(i - 2),
                b"<<" => {
                    depth -= 1;
                    i -= 2;
                }
                b">>" => {
                    depth += 1;
                    i -= 2;
                }
                _ => i -= 1,
            }
        };
        self.charge(at - i + 1);
        let start = start?;
        let hi = buf.len().min(at + PDF_DICT_WINDOW);
        let mut j = start;
        let end = loop {
            if j + 2 > hi {
                break None;
            }
            match &buf[j..j + 2] {
                b"<<" => {
                    depth += 1;
                    j += 2;
                }
                b">>" => {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break Some(j);
                    }
                }
                _ => j += 1,
            }
        };
        self.charge(j - start);
        if self.budget == 0 {
            return None;
        }
        end.map(|end| (start, end))
    }

    /// Each Flate `/Type /ObjStm` stream, inflated, with its place in the
    /// file. Streams that will not inflate are skipped; `None` when the
    /// budget runs out.
    fn object_streams(&mut self, doc: &[u8]) -> Option<Vec<(usize, Vec<u8>)>> {
        use std::io::Read;
        let mut out = Vec::new();
        for at in pdf_type_positions(doc, b"/ObjStm") {
            let Some((start, end)) = self.dict_around(doc, at) else {
                if self.budget == 0 {
                    return None;
                }
                continue;
            };
            if !pdf_has_key(&doc[start..end], b"/FlateDecode") {
                continue;
            }
            let Some(data) = self.stream_data(doc, end) else {
                if self.budget == 0 {
                    return None;
                }
                continue;
            };
            let mut inflated = Vec::new();
            let read = flate2::read::ZlibDecoder::new(data)
                .take(self.budget as u64 + 1)
                .read_to_end(&mut inflated);
            if inflated.len() >= self.budget {
                return None;
            }
            self.charge(inflated.len());
            if read.is_ok() {
                out.push((at, inflated));
            }
        }
        Some(out)
    }

    /// The bytes between `stream` and `endstream` after a stream dictionary
    /// ending at `dict_end`.
    fn stream_data<'a>(&mut self, doc: &'a [u8], dict_end: usize) -> Option<&'a [u8]> {
        let mut i = dict_end;
        while doc.get(i).is_some_and(|&b| pdf_is_white(b)) {
            i += 1;
        }
        let rest = doc.get(i..)?.strip_prefix(b"stream")?;
        let rest = rest
            .strip_prefix(b"\r\n")
            .or_else(|| rest.strip_prefix(b"\n"))
            .unwrap_or(rest);
        let window = &rest[..rest.len().min(self.budget)];
        let end = memchr::memmem::find(window, b"endstream");
        self.charge(end.unwrap_or(window.len()));
        end.map(|end| &rest[..end])
    }
}

fn pdf_is_white(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// A byte that continues a PDF name (neither whitespace nor a delimiter).
fn pdf_is_regular(b: u8) -> bool {
    !pdf_is_white(b) && !b"()<>[]{}/%".contains(&b)
}

/// Positions of each `/Type <name>` where `name` is the whole name:
/// `/Type /Page` never matches `/Type /Pages`.
fn pdf_type_positions<'a>(buf: &'a [u8], name: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    memchr::memmem::find_iter(buf, b"/Type").filter(move |&at| {
        let mut i = at + b"/Type".len();
        if buf.get(i).is_some_and(|&b| pdf_is_regular(b)) {
            return false;
        }
        while buf.get(i).is_some_and(|&b| pdf_is_white(b)) {
            i += 1;
        }
        buf[i..].starts_with(name) && !buf.get(i + name.len()).is_some_and(|&b| pdf_is_regular(b))
    })
}

/// Whether `dict` holds the whole name `key`.
fn pdf_has_key(dict: &[u8], key: &[u8]) -> bool {
    memchr::memmem::find_iter(dict, key)
        .any(|at| !dict.get(at + key.len()).is_some_and(|&b| pdf_is_regular(b)))
}

/// The page tree's `/Count`, read with at most five digits so no hostile
/// digit run is parsed; outside 1..=[`PDF_MAX_PAGES`] it is malformed.
fn pdf_count(dict: &[u8]) -> Option<u64> {
    let key = b"/Count";
    let at = memchr::memmem::find_iter(dict, key)
        .find(|&at| !dict.get(at + key.len()).is_some_and(|&b| pdf_is_regular(b)))?;
    let rest = &dict[at + key.len()..];
    let rest = &rest[rest.iter().take_while(|&&b| pdf_is_white(b)).count()..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if !(1..=5).contains(&digits) {
        return None;
    }
    let count: u64 = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
    (1..=PDF_MAX_PAGES).contains(&count).then_some(count)
}

/// Forward a request untouched through the standard pipeline. Shared by the
/// unparseable-body and unrouted-model exits below: both keep today's
/// behavior byte-identical.
async fn forward_unchanged(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = builder.body(Body::from(body)).expect("valid request");
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// Handle `POST /v1/messages/count_tokens`.
pub(crate) async fn handle_count_tokens(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // Not JSON — cannot name a routed model; forward byte-identical.
        Err(_) => {
            return forward_unchanged(state, client_addr, method, uri, headers, body).await;
        }
    };
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    if let Some(response) = try_answer_cursor_route(&state, &parsed, model) {
        return response;
    }
    // Only translated routes are unservable upstream (cursor routes are
    // handled above): their target speaks another protocol, so no model
    // string we could put here would count.
    // Everything else (real Anthropic models, non-translating passthrough
    // routes) forwards byte-identical and keeps the exact upstream count.
    let target = find_route_target(&state.config, model);
    let translated = target.as_ref().is_some_and(|t| t.translate);
    if let Some(response) = try_answer_passthrough_route(&parsed, model, target.as_ref()) {
        return response;
    }
    if !translated {
        return forward_unchanged(state, client_addr, method, uri, headers, body).await;
    }
    let input_tokens = estimate_input_tokens(&parsed, model, DocumentPricing::Placeholder);
    tracing::info!(
        event = "count_tokens_local",
        model = %model,
        input_tokens = input_tokens,
        "count_tokens for a translated route answered locally"
    );
    axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response()
}

/// Answer locally when the model rides a cursor route: cursor routes run a
/// subprocess, not HTTP, so no upstream could count these (forwarding would
/// hit the default Anthropic upstream with an unknown id: 404 plus the same
/// health-window pollution this endpoint exists to avoid).
/// `None` when this is not a cursor route — keep deciding.
/// Extracted from `handle_count_tokens` without behavior change.
fn try_answer_cursor_route(state: &AppState, parsed: &Value, model: &str) -> Option<Response> {
    // Cursor routes run a subprocess, not HTTP: no upstream could count
    // these, so answer locally like translated routes (forwarding would hit
    // the default Anthropic upstream with an unknown id: 404 plus the same
    // health-window pollution this endpoint exists to avoid).
    let agent = state
        .config
        .model_routes
        .iter()
        .find(|r| r.matches(model))
        .and_then(|r| r.resolve_cursor_agent(crate::output_shaper::requested_effort(parsed)))?;
    let input_tokens = estimate_input_tokens(parsed, &agent, DocumentPricing::Placeholder);
    tracing::info!(
        event = "count_tokens_local",
        model = %model,
        agent = %agent,
        input_tokens = input_tokens,
        "count_tokens for a cursor route answered locally"
    );
    Some(axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response())
}

/// Answer locally for a non-translating passthrough route with a known
/// Anthropic target: an approximate count without an upstream round trip
/// (the calibrated estimator undercounts slightly — framing is not
/// counted — so compaction may run a touch later than with a real
/// upstream count; the alternative today is a 404).
/// `None` when the route needs upstream handling — keep deciding.
/// Extracted from `handle_count_tokens` without behavior change.
fn try_answer_passthrough_route(
    parsed: &Value,
    model: &str,
    target: Option<&crate::routed::routing::RouteTarget>,
) -> Option<Response> {
    let t = target?;
    if !t.translate && t.target_model.is_some() {
        let input_tokens = estimate_input_tokens(parsed, model, DocumentPricing::Billed);
        tracing::info!(
            event = "count_tokens_local",
            model = %model,
            input_tokens = input_tokens,
            "count_tokens for an Anthropic-target route answered locally"
        );
        return Some(
            axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "claude-muse-spark-1.3";

    fn estimate(parsed: &Value) -> u64 {
        estimate_input_tokens(parsed, MODEL, DocumentPricing::Billed)
    }

    fn body() -> Value {
        serde_json::json!({
            "model": "claude-muse-spark-1.3",
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hello world, this is a test message"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "hi there"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/tmp/x"}},
                ]},
            ],
            "tools": [{"name": "Read", "description": "read a file"}],
        })
    }

    #[test]
    fn estimate_counts_system_messages_and_tools() {
        let base = estimate(&body());
        assert!(base > 0, "a real body counts for something");

        let mut more = body();
        more["messages"].as_array_mut().unwrap().push(
            serde_json::json!({"role": "user", "content": "hello world, this is a test message"}),
        );
        assert!(estimate(&more) > base, "more text counts more");

        let mut no_tools = body();
        no_tools.as_object_mut().unwrap().remove("tools");
        assert!(
            estimate(&no_tools) < base,
            "tool definitions ride along as input"
        );
    }

    #[test]
    fn estimate_weights_images_so_compaction_can_fire() {
        let mut with_image = body();
        // 1x1 PNG: decodes to real dimensions, weighs (w*h)/750 = 1 token min.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        with_image["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user",
                "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_b64}}],
            }));
        let base = estimate(&body());
        assert!(
            estimate(&with_image) > base,
            "image bytes must move the count or the client never compacts"
        );
    }

    #[test]
    fn tool_result_text_counts_recursively() {
        let mut block = body();
        block["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1",
                             "content": [{"type": "text", "text": "file contents here"}]}],
            }));
        assert!(estimate(&block) > estimate(&body()));
    }

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use std::io::Write as _;

    /// A classic PDF: header, objects numbered from 1, an xref table with
    /// their real offsets, and a trailer naming 1 as `/Root`.
    fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend(body);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        let size = objects.len() + 1;
        out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        out.extend(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        out
    }

    /// Four pages under a two-level tree (root `/Count 4`, a child node
    /// `/Count 3` with inherited resources), padded to ~156 KB of content.
    fn four_page_pdf() -> Vec<u8> {
        let page = |parent: u32| {
            format!("<< /Type /Page /Parent {parent} 0 R /Contents 9 0 R >>").into_bytes()
        };
        let text = "BT /F1 12 Tf 72 712 Td (padding) Tj ET\n".repeat(4000);
        let content = format!("<< /Length {} >>\nstream\n{text}\nendstream", text.len());
        pdf(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 7 0 R] /Count 4 >>".to_vec(),
            b"<< /Type /Pages /Parent 2 0 R /Kids [4 0 R 5 0 R 6 0 R] /Count 3 \
              /Resources << /Font << /F1 8 0 R >> >> >>"
                .to_vec(),
            page(3),
            page(3),
            page(3),
            page(2),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            content.into_bytes(),
        ])
    }

    fn pdf_block(doc: &[u8]) -> Value {
        serde_json::json!({"type": "document", "source": {
            "type": "base64", "media_type": "application/pdf", "data": STANDARD.encode(doc)}})
    }

    /// What one content block adds to a body without it.
    fn block_cost(block: Value, documents: DocumentPricing) -> u64 {
        let mut with = body();
        with["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role": "user", "content": [block]}));
        estimate_input_tokens(&with, MODEL, documents)
            - estimate_input_tokens(&body(), MODEL, documents)
    }

    fn by_size(len: usize) -> u64 {
        (len as u64 / 3000).max(1) * DOCUMENT_TOKENS_PER_PAGE
    }

    /// A 4-page, 156 KB PDF used to weigh decoded bytes/750 = ~208 tokens;
    /// Anthropic bills ~1,500 per page.
    #[test]
    fn pdf_is_priced_by_its_page_tree() {
        let doc = four_page_pdf();
        assert!(doc.len() > 150_000);
        assert_eq!(
            block_cost(pdf_block(&doc), DocumentPricing::Billed),
            4 * 1500
        );
        // Claude Code's Read tool returns a PDF inside a tool_result.
        let read = serde_json::json!({"type": "tool_result", "tool_use_id": "t1",
                                      "content": [pdf_block(&doc)]});
        assert_eq!(block_cost(read, DocumentPricing::Billed), 4 * 1500);
    }

    /// An incremental update appends a newer tree; the older one must not win.
    #[test]
    fn newest_page_tree_wins_after_an_incremental_update() {
        let mut doc = four_page_pdf();
        doc.extend(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 7 0 R 10 0 R] /Count 5 >>\nendobj\n");
        doc.extend(b"10 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n%%EOF\n");
        assert_eq!(pdf_tokens(&doc), 5 * 1500);
    }

    #[test]
    fn pdf_without_page_tree_counts_page_objects() {
        let page = b"<< /Type /Page /MediaBox [0 0 612 792] >>".to_vec();
        let doc = pdf(&[
            b"<< /Type /Catalog /Pages 9 0 R >>".to_vec(),
            page.clone(),
            page.clone(),
            page,
        ]);
        assert_eq!(
            block_cost(pdf_block(&doc), DocumentPricing::Billed),
            3 * 1500
        );
    }

    /// PDF 1.5+ writers (LaTeX, Word) pack the tree into a Flate object stream.
    #[test]
    fn page_tree_inside_an_object_stream_counts() {
        let kids: Vec<String> = (4..=10).map(|n| format!("{n} 0 R")).collect();
        let mut objects = vec![format!(
            "<< /Type /Pages /Kids [{}] /Count 7 >>",
            kids.join(" ")
        )];
        objects.extend((4..=10).map(|_| "<< /Type /Page /Parent 3 0 R >>".to_string()));
        let (mut header, mut packed) = (String::new(), Vec::new());
        for (i, object) in objects.iter().enumerate() {
            header.push_str(&format!("{} {} ", i + 3, packed.len()));
            packed.extend(object.bytes());
            packed.push(b'\n');
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&[header.as_bytes(), &packed].concat()).unwrap();
        let z = z.finish().unwrap();
        let dict = format!(
            "<< /Type /ObjStm /N {} /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
            objects.len(),
            header.len(),
            z.len()
        );
        let stream = [dict.as_bytes(), &z, b"\nendstream"].concat();
        let doc = pdf(&[b"<< /Type /Catalog /Pages 3 0 R >>".to_vec(), stream]);
        assert!(
            memchr::memmem::find(&doc, b"/Type /Page").is_none(),
            "tree is packed"
        );
        assert_eq!(
            block_cost(pdf_block(&doc), DocumentPricing::Billed),
            7 * 1500
        );
    }

    #[test]
    fn unreadable_pdfs_fall_back_without_panic() {
        let cost = |data: String| {
            block_cost(
                serde_json::json!({"type": "document", "source": {
                    "type": "base64", "media_type": "application/pdf", "data": data}}),
                DocumentPricing::Billed,
            )
        };
        assert_eq!(
            cost("%%% not base64 %%%".into()),
            1500,
            "undecodable: one page"
        );
        let text = b"not a pdf ".repeat(3000);
        assert_eq!(
            cost(STANDARD.encode(&text)),
            by_size(text.len()),
            "no header: size"
        );
        for count in ["0", "99999", "1000000", &"9".repeat(5000)] {
            let doc = format!("%PDF-1.7\n1 0 obj << /Type /Pages /Count {count} >> endobj");
            assert_eq!(
                pdf_tokens(doc.as_bytes()),
                by_size(doc.len()),
                "count {count}"
            );
        }
        // Every candidate dictionary walks the whole window: the budget ends it.
        let deep = [
            b"%PDF-1.7\n".as_slice(),
            &b">>".repeat(40_000),
            &b"/Type /Pages /Count 2 ".repeat(1000),
        ]
        .concat();
        assert_eq!(pdf_tokens(&deep), by_size(deep.len()));
        // A corrupt object stream is skipped; the pages outside it count.
        let corrupt = b"%PDF-1.7\n1 0 obj << /Type /ObjStm /Filter /FlateDecode >>\nstream\n\
            not zlib\nendstream endobj 2 0 obj << /Type /Page >> endobj 3 0 obj << /Type /Page >>";
        assert_eq!(pdf_tokens(corrupt), 2 * 1500);
        // An object stream inflating past the budget gives the size estimate.
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(&vec![0u8; 8 << 20]).unwrap();
        let bomb = [
            b"%PDF-1.7\n<< /Type /ObjStm /Filter /FlateDecode >>\nstream\n".as_slice(),
            &z.finish().unwrap(),
            b"\nendstream << /Type /Pages /Count 2 >>",
        ]
        .concat();
        assert_eq!(pdf_tokens(&bomb), by_size(bomb.len()));
        // Truncated anywhere: no panic.
        let doc = four_page_pdf();
        for cut in (0..doc.len()).step_by(997) {
            pdf_tokens(&doc[..cut]);
        }
    }

    #[test]
    fn text_documents_count_their_text() {
        let words = "the quick brown fox jumps over the lazy dog ".repeat(50);
        let block = serde_json::json!({"type": "document", "title": "notes",
            "source": {"type": "text", "media_type": "text/plain", "data": words}});
        let counted = block_cost(block, DocumentPricing::Billed);
        let text = headroom_core::tokenizer::get_tokenizer(MODEL).count_text(&(words + "notes"));
        assert!(counted.abs_diff(text as u64) <= 2, "{counted} vs {text}");
    }

    /// Translated and cursor routes send a placeholder line, not the PDF:
    /// pricing its pages there would compact the client early.
    #[test]
    fn routes_that_drop_documents_count_the_placeholder() {
        let block = pdf_block(&four_page_pdf());
        let dropped = block_cost(block.clone(), DocumentPricing::Placeholder);
        let line = headroom_core::tokenizer::get_tokenizer(MODEL).count_text(DOCUMENT_PLACEHOLDER);
        assert!(
            dropped > 0 && dropped.abs_diff(line as u64) <= 1,
            "{dropped}"
        );
        assert_eq!(block_cost(block, DocumentPricing::Billed), 4 * 1500);
        // Images ride translated routes as images: same weight either way.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let image = serde_json::json!({"type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": png}});
        assert_eq!(
            block_cost(image.clone(), DocumentPricing::Placeholder),
            block_cost(image, DocumentPricing::Billed)
        );
    }
}
