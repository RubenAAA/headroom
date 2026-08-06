//! Turn hooks — observe and optionally re-drive a single model turn.
//!
//! A *turn hook* wraps the proxy's buffered upstream call: it sees the outbound
//! request and the model's response, and may call the model again (via
//! `call_model`) to return a *replacement* response — transparently to the
//! client. That covers a range of turn-level behaviors an extension might want:
//! resolving an injected tool call, enforcing a guardrail, retrying on a bad
//! response, serving a cached answer, or running a small model→proxy→model loop
//! before handing back the final answer.
//!
//! This module is **inert unless a hook is registered**: the runner helpers
//! return their input unchanged, so with no hooks the proxy behaves exactly as
//! if this module did not exist. Hooks must never take the proxy down — a hook
//! that panics is logged and skipped.
//!
//! Ports `headroom/proxy/turn_hooks.py` (commit ec950f7e / #1891, #1896).

use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use serde_json::Value;

/// Re-drive the model with a message list; returns the provider's response JSON.
///
/// The exact message/response shapes are provider-native (Anthropic / OpenAI /
/// Google), matching whatever the surrounding handler already works with. This
/// is the same proxy-internal re-call path the buffered handler uses for its
/// CCR continuation loop, exposed to hooks.
#[async_trait]
pub trait CallModel: Send + Sync {
    async fn call(&self, messages: Vec<Value>) -> Value;
}

/// The request side of one model turn, as the proxy forwards it upstream.
///
/// `tools` and `messages` are the live values the handler is about to send (or
/// just sent); a hook's `on_request` may mutate them in place.
#[derive(Debug, Clone)]
pub struct TurnContext {
    /// `"anthropic" | "openai" | "google" | ...`
    pub provider: String,
    pub model: String,
    pub messages: Vec<Value>,
    /// Provider-native tools value (array, or `None`).
    pub tools: Option<Value>,
    /// Provider-native, untyped extra config carried alongside the turn.
    pub config: Option<Value>,
}

impl TurnContext {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        messages: Vec<Value>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            messages,
            tools: None,
            config: None,
        }
    }
}

/// A registered turn observer. Both methods are optional (a hook may override
/// either); the defaults are no-ops.
#[async_trait]
pub trait TurnHook: Send + Sync {
    /// A stable name used in log lines.
    fn name(&self) -> &str;

    /// Inspect / mutate `ctx` (e.g. `ctx.tools`) before it goes upstream.
    fn on_request(&self, _ctx: &mut TurnContext) {}

    /// Return a replacement response, or `None` to leave it unchanged.
    ///
    /// May call `call_model.call(messages)` to re-drive the model (e.g. to
    /// resolve an injected tool call), looping as needed before returning the
    /// final response.
    async fn on_response(
        &self,
        _ctx: &TurnContext,
        _response: &Value,
        _call_model: &dyn CallModel,
    ) -> Option<Value> {
        None
    }
}

// ─── Registry ────────────────────────────────────────────────────────────
//
// Mirrors the module-level registry convention already used by
// `interceptors::base::INTERCEPTORS` (a `LazyLock<Mutex<Vec<..>>>`).

static HOOKS: LazyLock<Mutex<Vec<Arc<dyn TurnHook>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn lock() -> std::sync::MutexGuard<'static, Vec<Arc<dyn TurnHook>>> {
    HOOKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a hook. Called by an extension's install path.
pub fn register_turn_hook(hook: Arc<dyn TurnHook>) {
    tracing::info!(hook = %hook.name(), "registered turn hook");
    lock().push(hook);
}

/// Snapshot of the registered hooks.
pub fn registered_turn_hooks() -> Vec<Arc<dyn TurnHook>> {
    lock().clone()
}

/// Test/reset helper.
pub fn clear_turn_hooks() {
    lock().clear();
}

// ─── Runners ─────────────────────────────────────────────────────────────

/// Run every hook's `on_request`. Inert when none are registered; a hook that
/// panics is logged and skipped so it can never take the proxy down.
pub fn run_request_hooks(ctx: &mut TurnContext) {
    let hooks = registered_turn_hooks();
    for hook in &hooks {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook.on_request(ctx);
        }));
        if result.is_err() {
            tracing::error!(hook = %hook.name(), "turn hook on_request panicked; skipped");
        }
    }
}

/// Run every hook's `on_response`, chaining any replacements.
///
/// Returns the (possibly replaced) response. Inert when no hooks are registered
/// (returns `response` unchanged); a hook that panics is logged and skipped.
pub async fn run_response_hooks(
    ctx: &TurnContext,
    response: Value,
    call_model: &dyn CallModel,
) -> Value {
    use futures::FutureExt;

    let hooks = registered_turn_hooks();
    let mut current = response;
    for hook in &hooks {
        let fut = std::panic::AssertUnwindSafe(hook.on_response(ctx, &current, call_model));
        match fut.catch_unwind().await {
            Ok(Some(replacement)) => current = replacement,
            Ok(None) => {}
            Err(_) => {
                tracing::error!(hook = %hook.name(), "turn hook on_response panicked; skipped");
            }
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The registry is process-global; serialize tests that touch it.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn ctx() -> TurnContext {
        TurnContext::new("anthropic", "claude-x", vec![])
    }

    /// A `call_model` that must never be invoked.
    struct NoopCallModel;
    #[async_trait]
    impl CallModel for NoopCallModel {
        async fn call(&self, _messages: Vec<Value>) -> Value {
            panic!("call_model must not be invoked when no hook re-drives");
        }
    }

    // ── inert-when-empty (the load-bearing guarantee) ──────────────────

    #[test]
    fn request_runner_inert_when_empty() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();
        assert!(registered_turn_hooks().is_empty());
        let mut c = ctx();
        c.tools = Some(serde_json::json!([{"name": "a"}]));
        let before = c.tools.clone();
        run_request_hooks(&mut c);
        assert_eq!(c.tools, before);
    }

    #[tokio::test]
    async fn response_runner_returns_input_unchanged_when_empty() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();
        let resp = serde_json::json!({"id": "orig", "content": []});
        let out = run_response_hooks(&ctx(), resp.clone(), &NoopCallModel).await;
        assert_eq!(out, resp);
    }

    // ── on_request mutation ────────────────────────────────────────────

    #[test]
    fn on_request_may_mutate_ctx() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct Shrink;
        impl TurnHook for Shrink {
            fn name(&self) -> &str {
                "shrink"
            }
            fn on_request(&self, ctx: &mut TurnContext) {
                if let Some(Value::Array(tools)) = &mut ctx.tools {
                    tools.retain(|t| t.get("name").and_then(Value::as_str) != Some("drop_me"));
                }
            }
        }

        register_turn_hook(Arc::new(Shrink));
        let mut c = ctx();
        c.tools = Some(serde_json::json!([{"name": "keep"}, {"name": "drop_me"}]));
        run_request_hooks(&mut c);
        assert_eq!(c.tools, Some(serde_json::json!([{"name": "keep"}])));
        clear_turn_hooks();
    }

    // ── on_response replacement + re-drive loop ────────────────────────

    #[tokio::test]
    async fn on_response_can_replace_via_call_model() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct CountingCallModel {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl CallModel for CountingCallModel {
            async fn call(&self, _messages: Vec<Value>) -> Value {
                self.calls.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({"id": "resolved", "content": [{"type": "text", "text": "done"}]})
            }
        }

        struct ResolveOnce;
        #[async_trait]
        impl TurnHook for ResolveOnce {
            fn name(&self) -> &str {
                "resolve"
            }
            async fn on_response(
                &self,
                ctx: &TurnContext,
                response: &Value,
                call_model: &dyn CallModel,
            ) -> Option<Value> {
                if response.get("id").and_then(Value::as_str) == Some("needs-work") {
                    let mut messages = ctx.messages.clone();
                    messages.push(serde_json::json!({"role": "user", "content": "go"}));
                    return Some(call_model.call(messages).await);
                }
                None
            }
        }

        register_turn_hook(Arc::new(ResolveOnce));
        let cm = CountingCallModel {
            calls: AtomicUsize::new(0),
        };
        let mut c = ctx();
        c.messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let out = run_response_hooks(&c, serde_json::json!({"id": "needs-work"}), &cm).await;
        assert_eq!(out.get("id").and_then(Value::as_str), Some("resolved"));
        assert_eq!(cm.calls.load(Ordering::SeqCst), 1);
        clear_turn_hooks();
    }

    #[tokio::test]
    async fn on_response_none_leaves_response_unchanged() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct Observer;
        #[async_trait]
        impl TurnHook for Observer {
            fn name(&self) -> &str {
                "observe"
            }
            async fn on_response(
                &self,
                _ctx: &TurnContext,
                _response: &Value,
                _call_model: &dyn CallModel,
            ) -> Option<Value> {
                None
            }
        }

        register_turn_hook(Arc::new(Observer));
        let resp = serde_json::json!({"id": "orig"});
        let out = run_response_hooks(&ctx(), resp.clone(), &NoopCallModel).await;
        assert_eq!(out, resp);
        clear_turn_hooks();
    }

    #[tokio::test]
    async fn replacements_chain_across_hooks() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct First;
        #[async_trait]
        impl TurnHook for First {
            fn name(&self) -> &str {
                "first"
            }
            async fn on_response(
                &self,
                _ctx: &TurnContext,
                response: &Value,
                _call_model: &dyn CallModel,
            ) -> Option<Value> {
                Some(serde_json::json!({"id": "after-first", "seen": response["id"]}))
            }
        }

        struct Second;
        #[async_trait]
        impl TurnHook for Second {
            fn name(&self) -> &str {
                "second"
            }
            async fn on_response(
                &self,
                _ctx: &TurnContext,
                response: &Value,
                _call_model: &dyn CallModel,
            ) -> Option<Value> {
                Some(serde_json::json!({"id": "after-second", "seen": response["id"]}))
            }
        }

        register_turn_hook(Arc::new(First));
        register_turn_hook(Arc::new(Second));
        let out =
            run_response_hooks(&ctx(), serde_json::json!({"id": "orig"}), &NoopCallModel).await;
        assert_eq!(
            out,
            serde_json::json!({"id": "after-second", "seen": "after-first"})
        );
        clear_turn_hooks();
    }

    // ── a failing hook must never break the proxy ──────────────────────

    #[test]
    fn failing_on_request_is_swallowed() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct Boom;
        impl TurnHook for Boom {
            fn name(&self) -> &str {
                "boom"
            }
            fn on_request(&self, _ctx: &mut TurnContext) {
                panic!("kaboom");
            }
        }

        register_turn_hook(Arc::new(Boom));
        run_request_hooks(&mut ctx()); // must not panic
        clear_turn_hooks();
    }

    #[tokio::test]
    async fn failing_on_response_is_skipped_and_survivor_runs() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct Boom;
        #[async_trait]
        impl TurnHook for Boom {
            fn name(&self) -> &str {
                "boom"
            }
            async fn on_response(
                &self,
                _ctx: &TurnContext,
                _response: &Value,
                _call_model: &dyn CallModel,
            ) -> Option<Value> {
                panic!("kaboom");
            }
        }

        struct Good;
        #[async_trait]
        impl TurnHook for Good {
            fn name(&self) -> &str {
                "good"
            }
            async fn on_response(
                &self,
                _ctx: &TurnContext,
                _response: &Value,
                _call_model: &dyn CallModel,
            ) -> Option<Value> {
                Some(serde_json::json!({"id": "recovered"}))
            }
        }

        register_turn_hook(Arc::new(Boom));
        register_turn_hook(Arc::new(Good));
        let out =
            run_response_hooks(&ctx(), serde_json::json!({"id": "orig"}), &NoopCallModel).await;
        assert_eq!(out, serde_json::json!({"id": "recovered"}));
        clear_turn_hooks();
    }

    // ── hooks that only define one method rely on trait defaults ────────

    #[tokio::test]
    async fn hook_without_on_response_is_skipped() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_turn_hooks();

        struct OnlyRequest;
        impl TurnHook for OnlyRequest {
            fn name(&self) -> &str {
                "only-request"
            }
            fn on_request(&self, _ctx: &mut TurnContext) {}
        }

        register_turn_hook(Arc::new(OnlyRequest));
        let resp = serde_json::json!({"id": "orig"});
        let out = run_response_hooks(&ctx(), resp.clone(), &NoopCallModel).await;
        assert_eq!(out, resp);
        clear_turn_hooks();
    }
}
