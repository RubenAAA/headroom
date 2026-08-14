//! Upstream rejections — how often the provider refuses what the proxy sent.
//!
//! # The blind spot this closes
//!
//! A rejected turn is the most expensive outcome there is: the work is lost,
//! the client retries, and the cached prefix is written again. Yet the only
//! trace was one `event = "upstream_rejected"` line per request in a log
//! nobody greps, with no denominator. A defect that refused **22.5% of
//! subagent turns** ran for a day looking exactly like an unlucky afternoon.
//!
//! So this module keeps the ratio, not just the count, and says so out loud
//! when it climbs.
//!
//! # What counts
//!
//! Rate limiting is excluded. A 429 is the provider throttling a healthy
//! proxy; folding it in would put the alert permanently at the mercy of the
//! account's quota. 5xx is counted but never escalated — that is the provider
//! failing, not the proxy sending something unusable. The alert fires on
//! **4xx other than 429**, which is the class that means "we sent something
//! the API would not accept".

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_STATUS, METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL,
    METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL_HELP, METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL,
    METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL_HELP,
};

/// Responses the ratio is measured over. Small enough that a burst shows up
/// within a minute or two of live traffic, large enough that one bad request
/// in a quiet period does not trip it.
const WINDOW: usize = 50;
/// Fraction of the window that must be refused before the proxy says so.
const ALERT_RATE: f64 = 0.10;
/// Below this many rejections in the window the rate is noise, whatever it
/// computes to — three in fifty is a pattern, one in five is an accident.
const ALERT_MIN_REJECTIONS: usize = 3;
/// Gap between escalations. The condition persists for as long as the defect
/// does, and one line every five minutes is a signal; one per request is a
/// log flood that hides the thing it is reporting.
const ALERT_COOLDOWN: Duration = Duration::from_secs(300);

fn responses_total(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL,
            METRIC_PROXY_UPSTREAM_RESPONSES_TOTAL_HELP,
        )
        .expect("proxy_upstream_responses_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_upstream_responses_total registers exactly once");
        c
    })
}

fn rejections_total(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL,
                METRIC_PROXY_UPSTREAM_REJECTIONS_TOTAL_HELP,
            ),
            &[LABEL_STATUS],
        )
        .expect("proxy_upstream_rejections_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_upstream_rejections_total registers exactly once");
        c
    })
}

/// Lifetime totals, kept outside prometheus so `/cache-health` can read them
/// without walking the registry.
static RESPONSES: AtomicU64 = AtomicU64::new(0);
static REJECTIONS: AtomicU64 = AtomicU64::new(0);

/// The rolling window plus the last thing the provider said.
struct Window {
    /// One slot per recent response; `true` = refused (4xx other than 429).
    recent: [bool; WINDOW],
    next: usize,
    filled: usize,
    refused: usize,
    last_alert: Option<Instant>,
    last_status: u16,
    last_error_type: String,
    last_error_message: String,
}

impl Window {
    fn new() -> Self {
        Self {
            recent: [false; WINDOW],
            next: 0,
            filled: 0,
            refused: 0,
            last_alert: None,
            last_status: 0,
            last_error_type: String::new(),
            last_error_message: String::new(),
        }
    }

    fn push(&mut self, refused: bool) {
        if self.filled == WINDOW && self.recent[self.next] {
            self.refused -= 1;
        }
        self.recent[self.next] = refused;
        if refused {
            self.refused += 1;
        }
        self.next = (self.next + 1) % WINDOW;
        self.filled = (self.filled + 1).min(WINDOW);
    }

    fn rate(&self) -> f64 {
        if self.filled == 0 {
            return 0.0;
        }
        self.refused as f64 / self.filled as f64
    }

    /// Whether this observation should escalate: enough evidence, over the
    /// threshold, and not inside the cooldown of the last one.
    fn should_alert(&mut self, now: Instant) -> bool {
        if self.refused < ALERT_MIN_REJECTIONS || self.rate() < ALERT_RATE {
            return false;
        }
        if self
            .last_alert
            .is_some_and(|t| now.duration_since(t) < ALERT_COOLDOWN)
        {
            return false;
        }
        self.last_alert = Some(now);
        true
    }
}

fn window() -> &'static Mutex<Window> {
    static WINDOW_STATE: OnceLock<Mutex<Window>> = OnceLock::new();
    WINDOW_STATE.get_or_init(|| Mutex::new(Window::new()))
}

/// A 4xx that says the proxy sent something unusable. 429 is the provider
/// throttling and is deliberately not in this class.
fn is_proxy_fault(status: u16) -> bool {
    (400..500).contains(&status) && status != 429
}

/// Record what the provider said when it refused, so the escalation below can
/// name the failure rather than just its rate. Called from the rejection path,
/// which reads the error body; [`observe_upstream_response`] then counts the
/// response itself.
pub fn observe_rejection_reason(status: u16, error_type: &str, error_message: &str) {
    let mut w = window().lock().unwrap_or_else(|p| p.into_inner());
    w.last_status = status;
    w.last_error_type = error_type.to_string();
    w.last_error_message = error_message.chars().take(300).collect();
}

/// Record one upstream response, whatever its status.
///
/// Called for every response the proxy gets back, because a rejection count
/// without a total is the number that let this go unnoticed.
pub fn observe_upstream_response(status: u16) {
    let registry = super::prometheus::registry();
    responses_total(registry).inc();
    RESPONSES.fetch_add(1, Ordering::Relaxed);

    let rejected = !(200..300).contains(&status);
    if rejected {
        rejections_total(registry)
            .with_label_values(&[&status.to_string()])
            .inc();
        REJECTIONS.fetch_add(1, Ordering::Relaxed);
    }

    let mut w = window().lock().unwrap_or_else(|p| p.into_inner());
    w.push(is_proxy_fault(status));
    if w.should_alert(Instant::now()) {
        // `error` rather than `warn`: every individual rejection already logs a
        // warn, and the point of this line is that it is not one of those.
        tracing::error!(
            event = "upstream_rejection_rate_high",
            rejected = w.refused,
            of_last = w.filled,
            rate_pct = format!("{:.1}", w.rate() * 100.0),
            status = w.last_status,
            error_type = %w.last_error_type,
            error_message = %w.last_error_message,
            "upstream is refusing a large share of forwarded requests; \
             the proxy is likely sending something unusable"
        );
    }
}

/// Health summary for `/cache-health`, so the rate is visible where the
/// cache numbers already are rather than in a log.
pub fn snapshot() -> serde_json::Value {
    let w = window().lock().unwrap_or_else(|p| p.into_inner());
    let rate = w.rate();
    let verdict = if w.refused < ALERT_MIN_REJECTIONS {
        "healthy"
    } else if rate >= ALERT_RATE {
        "refusing"
    } else {
        "elevated"
    };
    serde_json::json!({
        "responses_total": RESPONSES.load(Ordering::Relaxed),
        "rejections_total": REJECTIONS.load(Ordering::Relaxed),
        "recent_window": w.filled,
        "recent_refused": w.refused,
        "recent_refused_pct": (rate * 1000.0).round() / 10.0,
        "last_status": w.last_status,
        "last_error_type": w.last_error_type,
        "last_error_message": w.last_error_message,
        "verdict": verdict,
        // The splice defects that produced the refusals this exists to catch.
        // Read together: a non-zero count here beside a "refusing" verdict is
        // the whole diagnosis.
        "ccr_unusable_blocks": super::ccr_splice::unusable_blocks_get(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rate limiting is the provider throttling a healthy proxy. Counting it
    /// as a fault would pin the alert to the account's quota.
    #[test]
    fn a_429_is_not_a_proxy_fault() {
        assert!(!is_proxy_fault(429));
        assert!(is_proxy_fault(400));
        assert!(is_proxy_fault(413));
        assert!(!is_proxy_fault(200));
        assert!(!is_proxy_fault(503), "a provider fault is not ours");
    }

    #[test]
    fn the_window_forgets_beyond_its_length() {
        let mut w = Window::new();
        for _ in 0..WINDOW {
            w.push(true);
        }
        assert_eq!(w.rate(), 1.0);
        for _ in 0..WINDOW {
            w.push(false);
        }
        assert_eq!(w.refused, 0, "the bad run must age out");
        assert_eq!(w.rate(), 0.0);
    }

    /// The shape that went unnoticed: roughly one turn in five refused. It has
    /// to trip the alert well before a day's worth of traffic.
    #[test]
    fn a_one_in_five_rejection_rate_escalates() {
        let mut w = Window::new();
        let now = Instant::now();
        for i in 0..20 {
            w.push(i % 5 == 0);
        }
        assert!(w.should_alert(now), "4 of 20 refused must escalate");
    }

    #[test]
    fn a_single_rejection_does_not_escalate() {
        let mut w = Window::new();
        w.push(true);
        assert!(
            !w.should_alert(Instant::now()),
            "one refusal is an accident, not a pattern"
        );
    }

    /// The condition lasts as long as the defect, so without a cooldown this
    /// would print once per request and bury itself.
    #[test]
    fn escalation_is_rate_limited() {
        let mut w = Window::new();
        let now = Instant::now();
        for i in 0..20 {
            w.push(i % 5 == 0);
        }
        assert!(w.should_alert(now));
        assert!(!w.should_alert(now + Duration::from_secs(60)));
        assert!(w.should_alert(now + ALERT_COOLDOWN + Duration::from_secs(1)));
    }
}
