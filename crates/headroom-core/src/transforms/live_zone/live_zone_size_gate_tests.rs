use super::*;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct CeilingGuard(Option<String>);

impl CeilingGuard {
    fn set(value: &str) -> Self {
        let prior = std::env::var("HEADROOM_KOMPRESS_MAX_TOKENS").ok();
        unsafe { std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", value) };
        Self(prior)
    }
}

impl Drop for CeilingGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => unsafe { std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", v) },
            None => unsafe { std::env::remove_var("HEADROOM_KOMPRESS_MAX_TOKENS") },
        }
    }
}

/// The gate has to live on the path the proxy actually uses. `live_zone`
/// dispatch is that path — `content_router::apply_strategy` is not called
/// by the proxy at all, so gating only there protects nothing in
/// production.
#[test]
fn an_oversized_plaintext_block_never_reaches_ml() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set("10");

    // 41 chars > 10 tokens * 4.
    let text = "x".repeat(41);
    let result =
        dispatch_compressor_uncached(&text, ContentType::PlainText, &DispatchConfig::default());

    assert!(
        matches!(result, DispatchResult::NoOp { .. }),
        "an oversized block must be skipped without reaching the model"
    );
}

/// Exactly at the ceiling is within it — the comparison is strictly
/// greater-than, matching Python.
#[test]
fn a_block_at_the_ceiling_is_not_gated() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set("10");

    assert!(!super::super::content_router::kompress_size_gate_exceeded(
        &"x".repeat(40)
    ));
    assert!(super::super::content_router::kompress_size_gate_exceeded(
        &"x".repeat(41)
    ));
}

/// A zero ceiling disables the gate, so even a huge block takes the normal
/// path (which then passes through when no model is cached).
#[test]
fn a_zero_ceiling_disables_the_gate_here_too() {
    let _lock = env_lock();
    let _guard = CeilingGuard::set("0");

    assert!(!super::super::content_router::kompress_size_gate_exceeded(
        &"x".repeat(1_000_000)
    ));
}
