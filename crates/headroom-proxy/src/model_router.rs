//! Cost-aware model routing (issue #1706).
//!
//! Complementary to content compression: route a request to a cheaper (or
//! more capable) model based on request characteristics, so callers can
//! stretch quota and control spend without changing their client.
//!
//! This is an opt-in, config-driven **mechanism**, not an opinionated
//! built-in policy. The operator declares an ordered list of rules mapping
//! request characteristics to a target model; the router picks the first
//! rule whose conditions all match and records the decision, with a
//! human-readable reason, so routing is observable and never a black box.
//! When disabled (the default) or when no rule matches, the original model
//! is returned unchanged, so behavior is identical to today.
//!
//! The router is a pure component: no I/O, no global state, fully
//! unit-testable. Wiring into the request path (reading the decision,
//! rewriting the outgoing model, logging, and metrics) lives in the
//! handlers.
//!
//! Ports Python's `headroom/proxy/model_router.py`.

use bytes::Bytes;
use serde_json::Value;

/// One ordered routing rule.
///
/// A rule matches when every condition that is set is satisfied (logical
/// AND). Conditions left as `None`/empty are ignored. Rules are evaluated
/// in order and the first match wins.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelRoute {
    /// Model to route to when this rule matches.
    pub to_model: String,
    /// Match only when estimated input tokens are <= this (cheap for small requests).
    pub max_input_tokens: Option<u64>,
    /// Match only when estimated input tokens are >= this.
    pub min_input_tokens: Option<u64>,
    /// Match only when the request declares no tools (a proxy for low-risk work).
    pub require_no_tools: bool,
    /// Restrict this rule to these source models. Empty = any source model.
    pub from_models: Vec<String>,
    /// Human-readable label surfaced in decision logs.
    pub name: String,
}

impl ModelRoute {
    /// True when every set condition is satisfied for this request.
    pub fn matches(&self, model: &str, input_tokens: u64, has_tools: bool) -> bool {
        if !self.from_models.is_empty() && !self.from_models.iter().any(|m| m == model) {
            return false;
        }
        if self.require_no_tools && has_tools {
            return false;
        }
        if let Some(max) = self.max_input_tokens {
            if input_tokens > max {
                return false;
            }
        }
        if let Some(min) = self.min_input_tokens {
            if input_tokens < min {
                return false;
            }
        }
        // A rule whose `to_model` equals the current model still MATCHES (strict
        // first-match-wins): it is a no-op (`changed` is false) that short-circuits
        // later rules, which lets an operator write an explicit exemption rule.
        true
    }
}

/// Configuration for [`ModelRouter`]. Disabled by default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelRouterConfig {
    pub enabled: bool,
    pub routes: Vec<ModelRoute>,
}

impl ModelRouterConfig {
    /// Build config from env-style strings, failing open to disabled.
    ///
    /// `routes_raw` is a JSON array of rule objects, e.g.:
    ///
    /// ```json
    /// [{"name": "small->mini", "max_input_tokens": 4000,
    ///   "require_no_tools": true, "to_model": "gpt-5.4-mini"}]
    /// ```
    ///
    /// A malformed value logs a warning and disables routing rather than
    /// raising, so a bad config can never take the proxy down.
    pub fn from_env(enabled_raw: Option<&str>, routes_raw: Option<&str>) -> Self {
        let enabled = truthy(enabled_raw);
        let routes = parse_routes(routes_raw);
        if enabled && routes.is_empty() {
            tracing::warn!("model router enabled but no valid routes configured; disabling");
            return Self {
                enabled: false,
                routes: Vec::new(),
            };
        }
        Self {
            enabled: enabled && !routes.is_empty(),
            routes,
        }
    }
}

/// The outcome of a routing evaluation for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDecision {
    pub original_model: String,
    pub routed_model: String,
    pub matched: bool,
    pub reason: String,
    pub rule_name: String,
}

impl ModelDecision {
    /// True when the caller should rewrite the outgoing model.
    pub fn changed(&self) -> bool {
        self.matched && self.routed_model != self.original_model
    }

    /// A non-matching decision that leaves `model` in place.
    fn passthrough(model: &str, reason: &str) -> Self {
        Self {
            original_model: model.to_string(),
            routed_model: model.to_string(),
            matched: false,
            reason: reason.to_string(),
            rule_name: String::new(),
        }
    }
}

/// Selects an outgoing model from ordered, config-driven rules.
#[derive(Debug, Clone, Default)]
pub struct ModelRouter {
    config: ModelRouterConfig,
}

impl ModelRouter {
    pub fn new(config: Option<ModelRouterConfig>) -> Self {
        Self {
            config: config.unwrap_or_default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled && !self.config.routes.is_empty()
    }

    /// Return the routing decision for a request.
    ///
    /// Never fails: on a disabled router or no matching rule, returns a
    /// non-matching decision that leaves the original model in place.
    pub fn select(&self, model: &str, input_tokens: u64, has_tools: bool) -> ModelDecision {
        if !self.enabled() {
            return ModelDecision::passthrough(model, "router disabled");
        }
        if model.is_empty() {
            return ModelDecision::passthrough(model, "no source model");
        }

        for route in &self.config.routes {
            if route.matches(model, input_tokens, has_tools) {
                // Python formats `{route.name or route.to_model!r}`: the `!r`
                // conversion applies to the whole expression, so the label is
                // always quoted — including when `name` is set.
                let label = if route.name.is_empty() {
                    &route.to_model
                } else {
                    &route.name
                };
                let reason = format!(
                    "matched rule {}: {} -> {} (input_tokens={}, has_tools={})",
                    py_repr_str(label),
                    model,
                    route.to_model,
                    input_tokens,
                    py_bool(has_tools),
                );
                return ModelDecision {
                    original_model: model.to_string(),
                    routed_model: route.to_model.clone(),
                    matched: true,
                    reason,
                    rule_name: route.name.clone(),
                };
            }
        }
        ModelDecision::passthrough(model, "no rule matched")
    }
}

/// Cheap, tokenizer-free estimate of request input size, for routing only.
///
/// Uses a ~4-chars-per-token heuristic over the serialized message, tool,
/// and system content. `system` covers Anthropic's top-level `system` field
/// (string or content-block list), which is not part of `messages` but can
/// dominate request size, so omitting it would let a large system prompt
/// route as if the request were tiny. This is deliberately approximate: it
/// runs on the hot path purely to pick a route tier, so it must not pay for
/// a real tokenizer. It never fails.
///
/// Character counts mirror Python's `len(str(...))`, i.e. the length in
/// Unicode scalar values of the `str()`/`repr()` rendering of the value.
pub fn estimate_input_tokens(
    messages: Option<&Value>,
    tools: Option<&Value>,
    system: Option<&Value>,
) -> u64 {
    let mut chars: u64 = 0;
    if let Some(Value::Array(items)) = messages {
        for msg in items {
            let rendered = match msg {
                Value::Object(map) => {
                    let content = map.get("content");
                    match content {
                        Some(v) => py_str(v),
                        // `msg.get("content", "")` -> `str("")` -> length 0.
                        None => String::new(),
                    }
                }
                other => py_str(other),
            };
            chars += rendered.chars().count() as u64;
        }
    }
    if let Some(v) = tools {
        if py_truthy(v) {
            chars += py_str(v).chars().count() as u64;
        }
    }
    if let Some(v) = system {
        if py_truthy(v) {
            chars += py_str(v).chars().count() as u64;
        }
    }
    chars / 4
}

// ---------------------------------------------------------------------------
// Config parsing
// ---------------------------------------------------------------------------

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or("").trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "enable" | "enabled"
    )
}

/// Route object keys the parser understands. Anything else is a typo, and a
/// typo would silently widen the rule — so it is rejected.
const ALLOWED_ROUTE_KEYS: [&str; 6] = [
    "to_model",
    "max_input_tokens",
    "min_input_tokens",
    "require_no_tools",
    "from_models",
    "name",
];

fn parse_routes(routes_raw: Option<&str>) -> Vec<ModelRoute> {
    let raw = routes_raw.unwrap_or("");
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("invalid HEADROOM_MODEL_ROUTES JSON; ignoring: {e}");
            return Vec::new();
        }
    };
    let Value::Array(entries) = parsed else {
        tracing::warn!("HEADROOM_MODEL_ROUTES must be a JSON array; ignoring");
        return Vec::new();
    };

    entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| route_from_entry(entry, i))
        .collect()
}

/// A route field was present but malformed: fail open and skip the route.
struct Invalid;

/// Parse one route object, failing open (skip) on any malformed condition.
///
/// A silently-broadened rule (e.g. an unparseable `max_input_tokens` treated
/// as "no cap") could route far more traffic than the operator intended, so
/// an invalid condition disables just that rule rather than widening it.
fn route_from_entry(entry: &Value, index: usize) -> Option<ModelRoute> {
    let Value::Object(map) = entry else {
        tracing::warn!("model route #{index} is not an object; skipping");
        return None;
    };
    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !ALLOWED_ROUTE_KEYS.contains(k))
        .collect();
    if !unknown.is_empty() {
        // A misspelled condition (e.g. "max_input_token") would otherwise be
        // ignored, silently widening the rule. Reject unknown keys instead.
        unknown.sort_unstable();
        tracing::warn!("model route #{index} has unknown key(s) {unknown:?}; skipping route");
        return None;
    }
    let to_model = match map.get("to_model") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => {
            tracing::warn!("model route #{index} missing string 'to_model'; skipping");
            return None;
        }
    };

    let max_tokens = strict_opt_int(map.get("max_input_tokens"), "max_input_tokens", index).ok()?;
    let min_tokens = strict_opt_int(map.get("min_input_tokens"), "min_input_tokens", index).ok()?;

    // Only an absent key defaults to `false`. Python reads
    // `entry.get("require_no_tools", False)` and then rejects anything that
    // is not a `bool`, so an explicit `null` skips the route.
    let require_no_tools = match map.get("require_no_tools") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            tracing::warn!(
                "model route #{index} 'require_no_tools' must be a boolean; skipping route"
            );
            return None;
        }
    };

    // As above: `entry.get("from_models", [])` only defaults when the key is
    // absent; an explicit `null` is not a list and skips the route.
    let from_models: Vec<String> = match map.get("from_models") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    _ => {
                        tracing::warn!(
                            "model route #{index} 'from_models' must be a list of strings; skipping route"
                        );
                        return None;
                    }
                }
            }
            out
        }
        Some(_) => {
            tracing::warn!(
                "model route #{index} 'from_models' must be a list of strings; skipping route"
            );
            return None;
        }
    };

    let name = match map.get("name") {
        None => String::new(),
        Some(v) => py_str(v),
    };

    Some(ModelRoute {
        to_model,
        max_input_tokens: max_tokens,
        min_input_tokens: min_tokens,
        require_no_tools,
        from_models,
        name,
    })
}

/// Return the int at `key`, `None` if absent, or `Err(Invalid)` if malformed.
///
/// Accepts JSON integers and digit strings; rejects booleans, floats, and
/// non-numeric values so a typo cannot silently remove a token bound.
fn strict_opt_int(value: Option<&Value>, key: &str, index: usize) -> Result<Option<u64>, Invalid> {
    let value = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let parsed: i128 = match value {
        Value::Bool(_) => {
            tracing::warn!("model route #{index} '{key}' must be an integer, not a boolean");
            return Err(Invalid);
        }
        Value::Number(n) => match n.as_i64() {
            Some(i) => i128::from(i),
            // A JSON float (or an integer beyond i64) renders via `str()` in
            // Python and then fails `int()`; floats always do, huge ints do
            // not — see the module tests for that documented divergence.
            None => match py_int_from_str(&n.to_string()) {
                Some(i) => i,
                None => {
                    tracing::warn!("model route #{index} '{key}' is not a valid integer");
                    return Err(Invalid);
                }
            },
        },
        other => {
            let rendered = py_str(other);
            match py_int_from_str(&rendered) {
                Some(i) => i,
                None => {
                    tracing::warn!("model route #{index} '{key}' is not a valid integer");
                    return Err(Invalid);
                }
            }
        }
    };
    if parsed < 0 {
        tracing::warn!("model route #{index} '{key}' must be non-negative");
        return Err(Invalid);
    }
    // Python's ints are unbounded; a bound above `u64::MAX` is unreachable
    // either way, so saturating keeps the match behaviour identical.
    Ok(Some(u64::try_from(parsed).unwrap_or(u64::MAX)))
}

/// Parse a string the way Python's `int(str)` does: surrounding whitespace is
/// stripped, an optional sign is allowed, and single underscores may separate
/// digits.
fn py_int_from_str(raw: &str) -> Option<i128> {
    let s = raw.trim();
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return None;
    }
    let mut cleaned = String::with_capacity(digits.len());
    let bytes: Vec<char> = digits.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c == '_' {
            // Underscores must sit between digits.
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = bytes.get(i + 1).is_some_and(char::is_ascii_digit);
            if !prev_digit || !next_digit {
                return None;
            }
            continue;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        cleaned.push(*c);
    }
    let magnitude: i128 = cleaned.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

// ---------------------------------------------------------------------------
// Python `str()` / `repr()` emulation
//
// The estimator's character counts come from `len(str(value))` on decoded
// JSON, so the Rust port has to render values the way Python does: a bare
// string for a top-level `str`, and `repr()` (single quotes, `True`/`False`/
// `None`, `{'k': v}` dicts) for everything nested.
// ---------------------------------------------------------------------------

fn py_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

/// Python's `str(value)`: a top-level string renders bare, everything else
/// renders as `repr()`.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => {
            let mut out = String::new();
            py_repr(other, &mut out);
            out
        }
    }
}

fn py_repr(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(b) => out.push_str(py_bool(*b)),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&py_repr_str(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&py_repr_str(k));
                out.push_str(": ");
                py_repr(v, out);
            }
            out.push('}');
        }
    }
}

/// Python's `repr()` for a `str`: single quotes unless the value contains a
/// single quote and no double quote, with the usual backslash escapes.
fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python truthiness for a decoded JSON value: empty containers, empty
/// strings, `0`, `false` and `null` are all falsy.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(to_model: &str) -> ModelRoute {
        ModelRoute {
            to_model: to_model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_by_default() {
        let router = ModelRouter::new(None);
        assert!(!router.enabled());
        let d = router.select("gpt-5.4", 10, false);
        assert!(!d.matched);
        assert!(!d.changed());
        assert_eq!(d.reason, "router disabled");
        assert_eq!(d.routed_model, "gpt-5.4");
    }

    #[test]
    fn first_match_wins_and_builds_reason() {
        let cfg = ModelRouterConfig {
            enabled: true,
            routes: vec![
                ModelRoute {
                    to_model: "mini".into(),
                    max_input_tokens: Some(100),
                    name: "small".into(),
                    ..Default::default()
                },
                route("never"),
            ],
        };
        let d = ModelRouter::new(Some(cfg)).select("gpt-5.4", 50, false);
        assert!(d.matched);
        assert!(d.changed());
        assert_eq!(d.routed_model, "mini");
        assert_eq!(d.rule_name, "small");
        assert_eq!(
            d.reason,
            "matched rule 'small': gpt-5.4 -> mini (input_tokens=50, has_tools=False)"
        );
    }

    #[test]
    fn unnamed_rule_reason_uses_target_model() {
        let cfg = ModelRouterConfig {
            enabled: true,
            routes: vec![route("mini")],
        };
        let d = ModelRouter::new(Some(cfg)).select("gpt-5.4", 7, true);
        assert_eq!(
            d.reason,
            "matched rule 'mini': gpt-5.4 -> mini (input_tokens=7, has_tools=True)"
        );
    }

    #[test]
    fn no_rule_matched() {
        let cfg = ModelRouterConfig {
            enabled: true,
            routes: vec![ModelRoute {
                to_model: "mini".into(),
                max_input_tokens: Some(10),
                ..Default::default()
            }],
        };
        let d = ModelRouter::new(Some(cfg)).select("gpt-5.4", 500, false);
        assert!(!d.matched);
        assert_eq!(d.reason, "no rule matched");
    }

    #[test]
    fn empty_model_short_circuits() {
        let cfg = ModelRouterConfig {
            enabled: true,
            routes: vec![route("mini")],
        };
        let d = ModelRouter::new(Some(cfg)).select("", 1, false);
        assert!(!d.matched);
        assert_eq!(d.reason, "no source model");
    }

    #[test]
    fn self_route_matches_but_is_not_a_change() {
        let cfg = ModelRouterConfig {
            enabled: true,
            routes: vec![route("gpt-5.4"), route("mini")],
        };
        let d = ModelRouter::new(Some(cfg)).select("gpt-5.4", 1, false);
        assert!(d.matched);
        assert!(!d.changed());
        assert_eq!(d.routed_model, "gpt-5.4");
    }

    #[test]
    fn conditions_are_anded() {
        let r = ModelRoute {
            to_model: "mini".into(),
            max_input_tokens: Some(100),
            min_input_tokens: Some(10),
            require_no_tools: true,
            from_models: vec!["a".into(), "b".into()],
            name: String::new(),
        };
        assert!(r.matches("a", 50, false));
        assert!(!r.matches("c", 50, false)); // from_models
        assert!(!r.matches("a", 50, true)); // require_no_tools
        assert!(!r.matches("a", 101, false)); // max
        assert!(!r.matches("a", 9, false)); // min
        assert!(r.matches("a", 10, false)); // inclusive bounds
        assert!(r.matches("a", 100, false));
    }

    // -- from_env ----------------------------------------------------------

    #[test]
    fn from_env_truthy_values() {
        for raw in ["1", "true", " TRUE ", "yes", "on", "enable", "enabled"] {
            let cfg = ModelRouterConfig::from_env(Some(raw), Some(r#"[{"to_model": "m"}]"#));
            assert!(cfg.enabled, "{raw} should enable");
        }
        for raw in ["0", "false", "", "off", "no", "  "] {
            let cfg = ModelRouterConfig::from_env(Some(raw), Some(r#"[{"to_model": "m"}]"#));
            assert!(!cfg.enabled, "{raw} should not enable");
            assert_eq!(cfg.routes.len(), 1);
        }
        assert!(!ModelRouterConfig::from_env(None, None).enabled);
    }

    #[test]
    fn enabled_without_routes_disables_and_drops_routes() {
        let cfg = ModelRouterConfig::from_env(Some("1"), Some("not json"));
        assert!(!cfg.enabled);
        assert!(cfg.routes.is_empty());
        let cfg = ModelRouterConfig::from_env(Some("1"), Some(r#"{"to_model": "m"}"#));
        assert!(!cfg.enabled);
        assert!(cfg.routes.is_empty());
    }

    #[test]
    fn parses_full_route() {
        let cfg = ModelRouterConfig::from_env(
            Some("1"),
            Some(
                r#"[{"name": "small->mini", "max_input_tokens": 4000,
                     "min_input_tokens": "10", "require_no_tools": true,
                     "from_models": ["gpt-5.4"], "to_model": "gpt-5.4-mini"}]"#,
            ),
        );
        assert!(cfg.enabled);
        assert_eq!(
            cfg.routes,
            vec![ModelRoute {
                to_model: "gpt-5.4-mini".into(),
                max_input_tokens: Some(4000),
                min_input_tokens: Some(10),
                require_no_tools: true,
                from_models: vec!["gpt-5.4".into()],
                name: "small->mini".into(),
            }]
        );
    }

    #[test]
    fn malformed_routes_are_skipped_individually() {
        let raw = r#"[
            "nope",
            {"to_model": ""},
            {"to_model": "a", "max_input_token": 5},
            {"to_model": "b", "max_input_tokens": 1.5},
            {"to_model": "c", "max_input_tokens": true},
            {"to_model": "d", "min_input_tokens": -1},
            {"to_model": "e", "require_no_tools": "yes"},
            {"to_model": "f", "from_models": "gpt"},
            {"to_model": "g", "from_models": [1]},
            {"to_model": "ok"}
        ]"#;
        let cfg = ModelRouterConfig::from_env(Some("1"), Some(raw));
        assert_eq!(
            cfg.routes
                .iter()
                .map(|r| r.to_model.as_str())
                .collect::<Vec<_>>(),
            vec!["ok"]
        );
    }

    #[test]
    fn null_token_bounds_are_absent_but_null_conditions_skip_the_route() {
        // `_strict_opt_int` explicitly treats an explicit `null` as "unset"...
        let cfg = ModelRouterConfig::from_env(
            Some("1"),
            Some(r#"[{"to_model": "m", "max_input_tokens": null, "min_input_tokens": null}]"#),
        );
        assert_eq!(cfg.routes, vec![route("m")]);

        // ...while `require_no_tools` and `from_models` only default when the
        // key is absent, so an explicit `null` fails their type check.
        for raw in [
            r#"[{"to_model": "m", "require_no_tools": null}]"#,
            r#"[{"to_model": "m", "from_models": null}]"#,
        ] {
            let cfg = ModelRouterConfig::from_env(Some("1"), Some(raw));
            assert!(cfg.routes.is_empty(), "{raw} should skip the route");
            assert!(!cfg.enabled);
        }
    }

    #[test]
    fn token_bounds_accept_the_forms_python_int_accepts() {
        let cases = [
            (r#""  42  ""#, Some(42u64)),
            (r#""1_0""#, Some(10)),
            (r#""+5""#, Some(5)),
            ("0", Some(0)),
            ("99999999999999999999999", Some(u64::MAX)), // saturated, see note
        ];
        for (literal, expected) in cases {
            let raw = format!(r#"[{{"to_model": "m", "max_input_tokens": {literal}}}]"#);
            let cfg = ModelRouterConfig::from_env(Some("1"), Some(&raw));
            assert_eq!(
                cfg.routes.first().and_then(|r| r.max_input_tokens),
                expected,
                "{literal}"
            );
        }
        for literal in [r#""-5""#, r#""4.0""#, "[1]", r#""""#] {
            let raw = format!(r#"[{{"to_model": "m", "max_input_tokens": {literal}}}]"#);
            assert!(
                ModelRouterConfig::from_env(Some("1"), Some(&raw))
                    .routes
                    .is_empty(),
                "{literal} should skip the route"
            );
        }
    }

    #[test]
    fn name_is_coerced_with_python_str() {
        for (literal, expected) in [("5", "5"), ("null", "None"), (r#"["a"]"#, "['a']")] {
            let raw = format!(r#"[{{"to_model": "m", "name": {literal}}}]"#);
            let cfg = ModelRouterConfig::from_env(Some("1"), Some(&raw));
            assert_eq!(cfg.routes[0].name, expected, "{literal}");
        }
    }

    // -- estimate_input_tokens --------------------------------------------

    #[test]
    fn estimate_counts_content_tools_and_system() {
        let messages = json!([{"role": "user", "content": "hello world"}]);
        assert_eq!(estimate_input_tokens(Some(&messages), None, None), 2);
    }

    #[test]
    fn estimate_ignores_non_list_messages() {
        assert_eq!(estimate_input_tokens(Some(&json!({"a": 1})), None, None), 0);
        assert_eq!(estimate_input_tokens(None, None, None), 0);
    }

    #[test]
    fn estimate_renders_block_content_like_python() {
        let messages = json!([{"content": [{"type": "text", "text": "hi"}]}]);
        // str([{'type': 'text', 'text': 'hi'}]) -> 32 chars -> 8 tokens.
        assert_eq!(estimate_input_tokens(Some(&messages), None, None), 8);
    }

    #[test]
    fn estimate_handles_non_dict_messages_and_missing_content() {
        let messages = json!(["raw", {"role": "user"}]);
        // `str("raw")` is bare (3 chars) and the content-less dict adds 0.
        assert_eq!(estimate_input_tokens(Some(&messages), None, None), 0);
        let messages = json!(["raw string here!"]);
        assert_eq!(estimate_input_tokens(Some(&messages), None, None), 4);
    }

    #[test]
    fn estimate_adds_tools_and_system() {
        let tools = json!([{"name": "x"}]);
        let system = json!("you are a helpful assistant");
        assert_eq!(estimate_input_tokens(None, Some(&tools), None), 3);
        assert_eq!(estimate_input_tokens(None, None, Some(&system)), 6);
        assert_eq!(estimate_input_tokens(None, Some(&tools), Some(&system)), 10);
    }

    #[test]
    fn estimate_skips_falsy_tools_and_system() {
        assert_eq!(
            estimate_input_tokens(None, Some(&json!([])), Some(&json!(""))),
            0
        );
        assert_eq!(estimate_input_tokens(None, Some(&Value::Null), None), 0);
    }

    // -- python rendering helpers -----------------------------------------

    #[test]
    fn py_str_matches_python() {
        assert_eq!(py_str(&json!("a'b")), "a'b");
        assert_eq!(py_str(&json!(["a'b"])), r#"["a'b"]"#);
        assert_eq!(py_str(&json!(["a\"b"])), "['a\"b']");
        assert_eq!(py_str(&json!([true, false, null])), "[True, False, None]");
        assert_eq!(py_str(&json!({"a": [1, 2]})), "{'a': [1, 2]}");
        assert_eq!(py_str(&json!(["a\nb"])), r"['a\nb']");
        assert_eq!(py_str(&json!([])), "[]");
        assert_eq!(py_str(&json!({})), "{}");
    }
}

/// Apply cost-aware model routing to an Anthropic `/v1/messages` body.
///
/// Mirrors Python's `_apply_model_routing`: estimate the request's size, ask
/// the router for a decision, and rewrite `body["model"]` only when the
/// decision actually changes it.
///
/// Fails open at every step — an unparseable body, a body that is not a JSON
/// object, or a missing `model` field all return the original bytes untouched.
/// Routing is an optimisation; it must never be able to break a request.
///
/// The caller is responsible for skipping this under bypass/passthrough, so a
/// byte-faithful request is never model-rewritten.
pub fn apply_to_anthropic_body(body: Bytes, router: &ModelRouter, request_id: &str) -> Bytes {
    if !router.enabled() {
        return body;
    }
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(obj) = parsed.as_object_mut() else {
        return body;
    };
    let Some(model) = obj.get("model").and_then(|m| m.as_str()) else {
        return body;
    };

    let input_tokens =
        estimate_input_tokens(obj.get("messages"), obj.get("tools"), obj.get("system"));
    let has_tools = obj
        .get("tools")
        .map(|t| match t {
            Value::Array(a) => !a.is_empty(),
            Value::Null => false,
            _ => true,
        })
        .unwrap_or(false);

    let decision = router.select(model, input_tokens, has_tools);
    tracing::info!(
        request_id = %request_id,
        reason = %decision.reason,
        "model routing decision"
    );
    if !decision.changed() {
        return body;
    }

    let routed = decision.routed_model.clone();
    obj.insert("model".to_string(), Value::String(routed));
    match serde_json::to_vec(&parsed) {
        Ok(v) => Bytes::from(v),
        // Re-serialisation cannot realistically fail for a value we just
        // parsed, but forwarding the original beats dropping the request.
        Err(_) => body,
    }
}

#[cfg(test)]
mod body_routing_tests {
    use super::*;
    use serde_json::json;

    fn router(routes_json: &str) -> ModelRouter {
        ModelRouter::new(Some(ModelRouterConfig::from_env(
            Some("1"),
            Some(routes_json),
        )))
    }

    fn model_of(body: &Bytes) -> String {
        let v: Value = serde_json::from_slice(body).expect("valid json");
        v["model"].as_str().unwrap_or_default().to_string()
    }

    fn body_with(model: &str, tools: Value) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": tools,
            }))
            .unwrap(),
        )
    }

    #[test]
    fn a_matching_small_request_is_rerouted() {
        let r = router(r#"[{"name":"small","max_input_tokens":100000,"to_model":"gpt-5.6-luna"}]"#);
        let out = apply_to_anthropic_body(body_with("claude-opus-5", Value::Null), &r, "req");
        assert_eq!(model_of(&out), "gpt-5.6-luna");
    }

    /// `require_no_tools` exists so tool-using requests stay on the stronger
    /// model; a request declaring tools must not be downrouted.
    #[test]
    fn a_tool_using_request_is_left_alone_when_the_rule_forbids_tools() {
        let r = router(r#"[{"name":"small","require_no_tools":true,"to_model":"gpt-5.6-luna"}]"#);
        let out = apply_to_anthropic_body(
            body_with("claude-opus-5", json!([{"name": "Read"}])),
            &r,
            "req",
        );
        assert_eq!(model_of(&out), "claude-opus-5");
    }

    /// An empty `tools` array is not "has tools" — otherwise clients that
    /// always send the key would never route.
    #[test]
    fn an_empty_tools_array_does_not_count_as_having_tools() {
        let r = router(r#"[{"name":"small","require_no_tools":true,"to_model":"gpt-5.6-luna"}]"#);
        let out = apply_to_anthropic_body(body_with("claude-opus-5", json!([])), &r, "req");
        assert_eq!(model_of(&out), "gpt-5.6-luna");
    }

    #[test]
    fn a_disabled_router_returns_the_body_untouched() {
        let r = ModelRouter::new(Some(ModelRouterConfig::from_env(
            Some("0"),
            Some(r#"[{"to_model":"gpt-5.6-luna"}]"#),
        )));
        let original = body_with("claude-opus-5", Value::Null);
        let out = apply_to_anthropic_body(original.clone(), &r, "req");
        assert_eq!(
            out, original,
            "bytes must be identical, not just equivalent"
        );
    }

    /// Fail-open: routing is an optimisation and must never break a request.
    #[test]
    fn malformed_input_is_passed_through_unchanged() {
        let r = router(r#"[{"to_model":"gpt-5.6-luna"}]"#);

        for original in [
            Bytes::from_static(b"not json at all"),
            Bytes::from_static(b"[1,2,3]"),
            Bytes::from_static(br#"{"messages":[]}"#),
        ] {
            let out = apply_to_anthropic_body(original.clone(), &r, "req");
            assert_eq!(out, original);
        }
    }

    /// A rule routing a model to itself is not a change, so the body should be
    /// returned byte-identical rather than re-serialised.
    #[test]
    fn routing_a_model_to_itself_does_not_rewrite() {
        let r = router(r#"[{"to_model":"claude-opus-5"}]"#);
        let original = body_with("claude-opus-5", Value::Null);
        let out = apply_to_anthropic_body(original.clone(), &r, "req");
        assert_eq!(out, original);
    }
}
