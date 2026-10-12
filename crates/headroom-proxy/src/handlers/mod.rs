//! Per-endpoint POST handlers wired explicitly into the router.
//!
//! Most paths flow through the catch-all `forward_http` which gates
//! compression on `compression::is_compressible_path` + content-type.
//! The handlers in this module exist for endpoints whose request
//! shape needs explicit routing for clarity (PR-C2 onward) — the
//! actual forwarding logic still goes through `forward_http`. The
//! gate in `forward_http` runs the per-provider live-zone dispatcher
//! based on `classify_compressible_path`.

pub mod batch;
pub mod batch_anthropic;
pub mod chat_completions;
pub mod conversations;
pub mod count_tokens;
pub mod gemini;
pub mod local_model;
pub mod model_catalog;
pub mod reasoning_signature;
pub mod responses;
pub mod retrieve;
pub mod route_resolve;
pub mod stats;

/// Client-facing text for a request body that failed to parse.
///
/// Fixed strings chosen by error kind (upstream a88e2d51, CodeQL
/// `py/stack-trace-exposure`): the parser's own text carries decoder
/// internals, so it goes to the log, not back to the caller.
pub(crate) fn invalid_request_body_message(e: &serde_json::Error) -> &'static str {
    tracing::info!(event = "request_body_rejected", error = %e, "rejected request body");
    match e.classify() {
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            "Invalid request body: malformed JSON"
        }
        _ => "Invalid request body",
    }
}

/// `what` for the caller; `what` plus the error for the log. Forward errors
/// can carry the upstream URL, query string included (Gemini's `?key=`).
pub(crate) fn upstream_failure_message(what: &str, e: &dyn std::fmt::Display) -> String {
    tracing::warn!(event = "handler_upstream_failure", error = %e, "{what}");
    what.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_do_not_echo_the_error_text() {
        let e = serde_json::from_slice::<serde_json::Value>(b"{\"a\": tru").unwrap_err();
        assert_eq!(
            invalid_request_body_message(&e),
            "Invalid request body: malformed JSON"
        );
        let e = serde_json::from_str::<u8>("\"x\"").unwrap_err();
        assert_eq!(invalid_request_body_message(&e), "Invalid request body");
        let msg =
            upstream_failure_message("Failed to forward batch request", &"https://h/?key=SECRET");
        assert_eq!(msg, "Failed to forward batch request");
    }
}
