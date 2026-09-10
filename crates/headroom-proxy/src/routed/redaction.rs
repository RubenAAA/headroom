//! Reversible redaction for the routed paths.
//!
//! Placeholders go upstream; originals are restored at the edge before
//! anything reaches the client.

use crate::routed::outcome::RoutedOutcomeContext;
use bytes::Bytes;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Reversible redaction: placeholders out, originals back before the client.
// ---------------------------------------------------------------------------

/// Redact one outbound body. True when anything was rewritten — only then do
/// the response arms get the store. A turn with nothing sensitive keeps the
/// zero-overhead path: no map snapshot, no restore pass.
pub(crate) fn maybe_redact_outbound(
    store: &crate::redact::RedactStore,
    session_key: &str,
    parsed: &mut Value,
    request_id: &str,
) -> bool {
    let report = crate::redact::redact_body(store, session_key, parsed);
    if report.spans_redacted > 0 {
        tracing::info!(
            event = "routed_redact_outbound",
            request_id = %request_id,
            spans_redacted = report.spans_redacted,
            placeholders_live = report.placeholders_live,
            "redacted sensitive spans before translation"
        );
    }
    report.spans_redacted > 0
}

/// Restore placeholders in a buffered body about to go to the client.
/// Nothing logged but counts: values stay in the map.
pub(crate) fn restore_buffered(
    outcome: Option<&RoutedOutcomeContext>,
    mut body: Vec<u8>,
) -> Vec<u8> {
    let request_id = outcome.map(|c| c.request_id.as_str()).unwrap_or("");
    let Some(ctx) = outcome else { return body };
    let Some(store) = ctx.redact_store.as_ref() else {
        return body;
    };
    let Some(table) = crate::redact::restore_table(store, &ctx.session_key) else {
        return body;
    };
    let (out, misses) = table.restore_bytes(&body);
    if misses > 0 {
        tracing::warn!(
            event = "routed_redact_restore_miss",
            request_id = %request_id,
            misses,
            "placeholders the map could not restore; left as-is"
        );
    }
    body = out;
    body
}

/// Wrap a translated SSE stream with placeholder restore. Chunk-boundary
/// safe: a token split across two chunks still restores. `None` passes the
/// stream through untouched.
pub(crate) fn restore_streaming<S, E>(
    table: Option<crate::redact::RestoreTable>,
    request_id: &str,
    stream: S,
) -> axum::body::Body
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    let Some(table) = table else {
        return axum::body::Body::from_stream(stream);
    };
    tracing::info!(
        event = "routed_redact_stream",
        request_id = %request_id,
        "restoring placeholders on the routed stream"
    );
    axum::body::Body::from_stream(crate::redact::restore_stream(stream, table))
}

/// Snapshot this turn's restore table, if it redacted. Taken before `outcome`
/// moves into the stream translator.
pub(crate) fn redact_table_for(
    outcome: Option<&RoutedOutcomeContext>,
) -> Option<crate::redact::RestoreTable> {
    let ctx = outcome?;
    let store = ctx.redact_store.as_ref()?;
    crate::redact::restore_table(store, &ctx.session_key)
}
