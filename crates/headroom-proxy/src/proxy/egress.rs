//! Upstream HTTP clients: the per-provider egress pool, per-caller client
//! cache, and the guard that holds an egress slot for a response's life.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

pub(super) const CALLER_CLIENT_CACHE_CAPACITY: usize = 128;

/// Client, slot, egress ID and in-flight guard for one routed Zen send.
pub(crate) type ZenEgressSelection<'a> = (
    &'a reqwest::Client,
    usize,
    &'a str,
    Option<EgressInflightGuard>,
);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CallerClientKey {
    pub(super) host: String,
    pub(super) addresses: Vec<SocketAddr>,
}

/// Provider-only transports assigned once per stream lane. First-seen lanes
/// cycle across the configured egresses, which gives a fan-out of N streams
/// N distinct egresses when at least N are configured; later turns stay on
/// their assigned egress. The bounded map retains affinity for active work
/// without growing forever on a long-lived proxy.
pub(crate) struct ProviderEgressPool {
    pub(super) clients: Vec<reqwest::Client>,
    pub(super) egress_ids: Vec<String>,
    pub(super) assignments: Mutex<ProviderEgressAssignments>,
    pub(super) maintenance: Mutex<std::collections::HashSet<String>>,
    /// Requests holding each egress, indexed like `clients`. Exposed on
    /// `/debug/inflight` as `egress_in_flight` so a rotation drains only the
    /// lane it rotates instead of waiting for the whole proxy to go idle.
    pub(super) in_flight: Vec<std::sync::atomic::AtomicUsize>,
}

pub(super) struct ProviderEgressAssignments {
    pub(super) lanes: lru::LruCache<String, usize>,
    pub(super) next_slot: usize,
}

impl ProviderEgressPool {
    pub(crate) fn new(clients: Vec<reqwest::Client>, egress_ids: Vec<String>) -> Self {
        debug_assert!(!clients.is_empty());
        debug_assert_eq!(clients.len(), egress_ids.len());
        Self {
            clients,
            assignments: Mutex::new(ProviderEgressAssignments {
                lanes: lru::LruCache::new(NonZeroUsize::new(4096).expect("non-zero capacity")),
                next_slot: 0,
            }),
            maintenance: Mutex::new(std::collections::HashSet::new()),
            in_flight: (0..egress_ids.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            egress_ids,
        }
    }

    pub(super) fn slot_for_lane(&self, lane_key: &str) -> usize {
        if lane_key.is_empty() {
            return 0;
        }
        let lane_fingerprint = hex::encode(Sha256::digest(lane_key.as_bytes()));
        let mut assignments = self.assignments.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&slot) = assignments.lanes.get(&lane_fingerprint) {
            return slot;
        }
        let slot = assignments.next_slot % self.clients.len();
        assignments.next_slot = assignments.next_slot.wrapping_add(1);
        assignments.lanes.put(lane_fingerprint, slot);
        slot
    }

    pub(crate) fn set_maintenance(&self, egress_id: &str, rotating: bool) -> bool {
        if !self.egress_ids.iter().any(|id| id == egress_id) {
            return false;
        }
        let mut maintenance = self.maintenance.lock().unwrap_or_else(|e| e.into_inner());
        if rotating {
            maintenance.insert(egress_id.to_owned());
        } else {
            maintenance.remove(egress_id);
        }
        true
    }

    pub(super) fn is_in_maintenance(&self, egress_id: &str) -> bool {
        self.maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(egress_id)
    }

    /// Count one request onto `slot`, or return its egress ID if the egress
    /// is rotating. The check and the increment happen under the lock
    /// `set_maintenance` takes, so once that call returns, every request it
    /// did not turn away is already counted and a drain that reads the count
    /// afterwards cannot miss it.
    pub(super) fn acquire(self: &Arc<Self>, slot: usize) -> Result<EgressInflightGuard, String> {
        let egress_id = &self.egress_ids[slot];
        let maintenance = self.maintenance.lock().unwrap_or_else(|e| e.into_inner());
        if maintenance.contains(egress_id) {
            return Err(egress_id.clone());
        }
        self.in_flight[slot].fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(maintenance);
        Ok(EgressInflightGuard {
            pool: Arc::clone(self),
            slot,
        })
    }

    pub(super) fn in_flight_by_egress(&self) -> serde_json::Map<String, serde_json::Value> {
        self.egress_ids
            .iter()
            .zip(&self.in_flight)
            .map(|(egress_id, count)| {
                (
                    egress_id.clone(),
                    count.load(std::sync::atomic::Ordering::SeqCst).into(),
                )
            })
            .collect()
    }
}

/// One request counted against one Zen egress. It goes with the response
/// body (see [`attach_egress_guard`]), so the count covers the whole turn up
/// to the last upstream byte, and drops early on error or client disconnect.
pub(crate) struct EgressInflightGuard {
    pub(super) pool: Arc<ProviderEgressPool>,
    pub(super) slot: usize,
}

impl Drop for EgressInflightGuard {
    fn drop(&mut self) {
        self.pool.in_flight[self.slot].fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pin_project_lite::pin_project! {
    /// Response body stream that releases its egress guard at the end of the
    /// body or on the first error, rather than whenever the consumer drops it.
    struct EgressGuardedStream<S> {
        #[pin]
        inner: S,
        guard: Option<EgressInflightGuard>,
    }
}

impl<S, T, E> futures_util::Stream for EgressGuardedStream<S>
where
    S: futures_util::Stream<Item = Result<T, E>>,
{
    type Item = Result<T, E>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        let item = futures_util::ready!(this.inner.poll_next(cx));
        if !matches!(item, Some(Ok(_))) {
            this.guard.take();
        }
        std::task::Poll::Ready(item)
    }
}

/// Move `guard` into `resp`'s body. Every consumer of a routed Zen response
/// (streamed, buffered, or dropped on an error arm) then holds the egress for
/// exactly as long as it holds the upstream body, with no plumbing per arm.
pub(crate) fn attach_egress_guard(
    resp: reqwest::Response,
    guard: Option<EgressInflightGuard>,
) -> reqwest::Response {
    let Some(guard) = guard else {
        return resp;
    };
    use http_body_util::BodyExt;
    use reqwest::ResponseBuilderExt;
    let url = resp.url().clone();
    let (parts, body) = http::Response::<reqwest::Body>::from(resp).into_parts();
    let body = reqwest::Body::wrap_stream(EgressGuardedStream {
        inner: body.into_data_stream(),
        guard: Some(guard),
    });
    let mut builder = http::Response::builder()
        .status(parts.status)
        .version(parts.version)
        .url(url);
    if let Some(headers) = builder.headers_mut() {
        *headers = parts.headers;
    }
    if let Some(extensions) = builder.extensions_mut() {
        extensions.extend(parts.extensions);
    }
    let rebuilt = builder
        .body(body)
        .expect("parts copied from a valid response");
    reqwest::Response::from(rebuilt)
}

pub(super) fn provider_egress_id(proxy_url: Option<&str>) -> String {
    let Some(proxy_url) = proxy_url else {
        return "direct".to_string();
    };
    let digest = Sha256::digest(proxy_url.as_bytes());
    format!("proxy-{}", hex::encode(&digest[..6]))
}

/// Transport settings shared by trusted and caller-selected upstreams.
///
/// Caller-selected destinations add a pinned DNS override and disable proxies
/// below, but retain the same TLS roots, timeouts, keepalives, redirect policy,
/// and response behavior as the normal upstream client.
pub(super) fn upstream_client_builder(config: &Config) -> reqwest::ClientBuilder {
    let builder = crate::ssl_context::client_builder()
        .connect_timeout(config.upstream_connect_timeout)
        // End-to-end bound on a single upstream request, streamed body
        // included. The send is bounded separately, below.
        .timeout(config.upstream_timeout)
        // Upstream redirects are forwarded to the client. In particular, a
        // caller-selected public endpoint cannot redirect this process into a
        // private network.
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(config.pool_idle_timeout)
        .http2_keep_alive_interval(std::time::Duration::from_secs(20))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .tcp_keepalive(std::time::Duration::from_secs(20));
    apply_upstream_write_timeout(builder, config)
}

/// Apply `config.upstream_write_timeout` (port of Python
/// `ProxyConfig.write_timeout_seconds`, upstream a507249b).
///
/// httpx bounds the send phase on its own; reqwest 0.12 has no per-phase
/// write knob, and both knobs it does have are the wrong phase — `timeout`
/// and `read_timeout` cover the wait for the answer, so setting either to
/// the write bound would kill a model that thinks longer than it. Linux's
/// `TCP_USER_TIMEOUT` bounds exactly what httpx's `write` bounds: outbound
/// bytes the peer never acknowledges, including a zero-window stall. Think
/// time is unaffected, because a thinking peer has already acked the
/// request. This replaces reqwest's own 30s default with the operator knob.
#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
pub(super) fn apply_upstream_write_timeout(
    builder: reqwest::ClientBuilder,
    config: &Config,
) -> reqwest::ClientBuilder {
    builder.tcp_user_timeout(config.upstream_write_timeout)
}

/// No `TCP_USER_TIMEOUT` off Linux: the send stays under the total
/// `upstream_timeout`, as it did before the knob existed.
#[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
pub(super) fn apply_upstream_write_timeout(
    builder: reqwest::ClientBuilder,
    _config: &Config,
) -> reqwest::ClientBuilder {
    builder
}

/// Build the request-scoped transport for a caller-selected upstream.
///
/// `resolve_to_addrs` preserves the URL hostname for Host/SNI while forcing
/// reqwest's connector to use the already-validated addresses. `no_proxy`
/// prevents an ambient or provider proxy from resolving the target a second
/// time beyond this process's policy boundary.
pub(super) fn caller_upstream_client(
    state: &AppState,
    upstream: &crate::upstream_guard::ResolvedCallerUpstream,
) -> Result<reqwest::Client, ProxyError> {
    let mut key_addresses = upstream.addresses().to_vec();
    key_addresses.sort_unstable();
    key_addresses.dedup();
    let key = CallerClientKey {
        host: upstream.host().to_string(),
        addresses: key_addresses,
    };
    // Fast path under a short critical section: never hold the mutex
    // across `Client::build()` (TLS/pool setup, potentially ms). Two
    // concurrent misses may both build; `put` is idempotent so the
    // loser simply overwrites with an equivalent client.
    if let Some(client) = state
        .caller_clients
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
    {
        return Ok(client);
    }

    let client = upstream_client_builder(&state.config)
        .no_proxy()
        .resolve_to_addrs(upstream.host(), upstream.addresses())
        .build()
        .map_err(ProxyError::Upstream)?;
    state
        .caller_clients
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .put(key, client.clone());
    Ok(client)
}
