//! Route resolution for `POST /v1/messages` (C2).
//!
//! Order-preserving extraction of the inline decision in
//! `handlers/local_model.rs`: cursor beats URL, first-match wins, legacy
//! `local_model` exact match before the table. The cost-router rewrite +
//! cooldown consultation stays in the caller, in today's order — this runs
//! on the post-rewrite body.

use serde_json::Value;

/// Where a post-rewrite model id goes.
pub(crate) enum RouteDecision {
    /// Run the Cursor agent CLI, not HTTP.
    Cursor { cursor_model: String },
    /// HTTP upstream, with translate/target/auth attached.
    Route {
        target: crate::routed::routing::RouteTarget,
    },
    /// No route claims this model; caller forwards.
    NoMatch,
}

/// Resolve `body_model` (post cost-rewrite) to a decision.
///
/// Cursor is checked before URL match: a `cursor:` route has no upstream URL
/// to match on. `cursor:`-only routes yield `NoMatch` from `find_route_target`
/// (no upstream), so they only serve via the `Cursor` arm above.
pub(crate) fn resolve_route(
    config: &crate::config::Config,
    parsed: &Value,
    body_model: &str,
) -> RouteDecision {
    let cursor_model = config
        .model_routes
        .iter()
        .find(|r| r.matches(body_model))
        .and_then(|r| r.resolve_cursor_agent(crate::output_shaper::requested_effort(parsed)));

    if let Some(cursor_model) = cursor_model {
        return RouteDecision::Cursor { cursor_model };
    }

    match crate::routed::routing::find_route_target(config, body_model) {
        Some(target) => RouteDecision::Route { target },
        None => RouteDecision::NoMatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(routes: Vec<crate::config::ProviderRoute>) -> crate::config::Config {
        let mut c = crate::config::Config::for_test("http://upstream:8080".parse().unwrap());
        c.model_routes = routes;
        c
    }

    fn route(
        prefix: &str,
        translate: bool,
        target: Option<&str>,
        cursor: Option<&str>,
        upstream: Option<&str>,
    ) -> crate::config::ProviderRoute {
        crate::config::ProviderRoute {
            model_prefix: prefix.to_string(),
            prefix_match: false,
            upstream: upstream.map(|u| u.parse().unwrap()),
            translate,
            cursor_agent: cursor.map(str::to_string),
            target_model: target.map(str::to_string),
            auth_env: None,
        }
    }

    fn parsed(model: &str) -> Value {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    /// Cursor beats URL: a cursor route never yields an HTTP target.
    #[test]
    fn cursor_beats_url_match() {
        let cfg = config_with(vec![route(
            "claude-grok-4.6",
            false,
            None,
            Some("grok-4.6-high"),
            None,
        )]);
        match resolve_route(&cfg, &parsed("claude-grok-4.6"), "claude-grok-4.6") {
            RouteDecision::Cursor { cursor_model } => {
                assert_eq!(cursor_model, "grok-4.6-high");
            }
            _ => panic!("cursor route must resolve to Cursor, never HTTP"),
        }
    }

    /// First-match wins: earlier table entry claims the model.
    #[test]
    fn first_match_wins() {
        let cfg = config_with(vec![
            route(
                "codex-*",
                true,
                Some("gpt-a"),
                None,
                Some("https://a.example/v1"),
            ),
            route(
                "codex-*",
                true,
                Some("gpt-b"),
                None,
                Some("https://b.example/v1"),
            ),
        ]);
        // Prefix match needs the flag; use exact dups instead.
        let mut cfg = cfg;
        cfg.model_routes[0].model_prefix = "codex-5.5".to_string();
        cfg.model_routes[0].prefix_match = false;
        cfg.model_routes[1].model_prefix = "codex-5.5".to_string();
        cfg.model_routes[1].prefix_match = false;
        match resolve_route(&cfg, &parsed("codex-5.5"), "codex-5.5") {
            RouteDecision::Route { target } => {
                assert_eq!(target.target_model.as_deref(), Some("gpt-a"));
            }
            _ => panic!("expected route"),
        }
    }

    /// `MODEL=cursor:X` style ids never yield HTTP when only a cursor route
    /// matches; unknown models fall through to the forwarder.
    #[test]
    fn unknown_model_is_no_match() {
        let cfg = config_with(vec![route(
            "claude-codex-5.5",
            true,
            Some("gpt-5.5"),
            None,
            Some("https://api.openai.com/v1"),
        )]);
        assert!(matches!(
            resolve_route(&cfg, &parsed("claude-opus-5"), "claude-opus-5"),
            RouteDecision::NoMatch
        ));
    }

    /// Legacy `local_model` exact match still claims before the table.
    #[test]
    fn legacy_local_model_wins() {
        let mut cfg = config_with(vec![route(
            "local",
            true,
            Some("gpt-x"),
            None,
            Some("https://table.example/v1"),
        )]);
        cfg.local_model = Some("local".to_string());
        cfg.local_upstream = Some("https://legacy.example/v1".parse().unwrap());
        match resolve_route(&cfg, &parsed("local"), "local") {
            RouteDecision::Route { target } => {
                assert_eq!(target.upstream.as_str(), "https://legacy.example/v1");
            }
            _ => panic!("legacy alias must win"),
        }
    }

    /// C7 matrix row: `translate==false` resolves to the passthrough variant
    /// (single verbatim POST, no retry/transforms/redaction/outcome) rather
    /// than the translate pipeline. The flag must survive resolution intact.
    #[test]
    fn translate_false_resolves_to_passthrough_variant() {
        let cfg = config_with(vec![route(
            "claude-passthrough",
            false,
            None,
            None,
            Some("https://api.meta.ai"),
        )]);
        match resolve_route(&cfg, &parsed("claude-passthrough"), "claude-passthrough") {
            RouteDecision::Route { target } => {
                assert!(!target.translate, "passthrough flag must survive");
                assert!(target.target_model.is_none());
            }
            _ => panic!("passthrough route must resolve to Route"),
        }
    }

    /// C7 matrix row: cursor resolves to the subprocess variant, never HTTP.
    /// The handler returns before outcome booking, so cursor turns book
    /// nothing — never the `$3/M` fallback (phantom spend).
    #[test]
    fn cursor_route_carries_no_http_upstream() {
        let cfg = config_with(vec![route(
            "claude-grok-4.6",
            false,
            None,
            Some("grok-4.6-high"),
            None,
        )]);
        // find_route_target yields None for cursor-only routes (no upstream);
        // only the Cursor arm serves them.
        assert!(
            crate::routed::routing::find_route_target(&cfg, "claude-grok-4.6").is_none(),
            "cursor-only routes must never yield an HTTP target"
        );
        assert!(matches!(
            resolve_route(&cfg, &parsed("claude-grok-4.6"), "claude-grok-4.6"),
            RouteDecision::Cursor { .. }
        ));
    }
}
