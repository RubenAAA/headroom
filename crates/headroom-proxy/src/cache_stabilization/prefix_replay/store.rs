//! Session and cross-session replay storage.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// One in-flight turn awaiting its response usage, so the response side can feed
/// cache tokens back into the right session's tracker.
#[derive(Clone, Debug)]
pub(super) struct PendingTurn {
    pub(super) session_key: String,
    pub(super) original_messages: Vec<Value>,
    pub(super) forwarded_messages: Vec<Value>,
    /// [`forwarded_system_digest`] of the system block the forwarded messages
    /// went out under. Recorded into the tracker at completion so the adoption
    /// gate can compare it against a later turn's post-hold system.
    pub(super) forwarded_system_hash: String,
}

/// One session's prefix, on disk, so a proxy restart does not throw it away.
///
/// # Why this is on disk at all
///
/// The store is in memory, so a restart empties it and the first turn of every
/// live conversation reports `no_previous_turn`. That is not a free miss.
/// [`PrefixReplayTracker::frozen_message_count`] returns 0 without a tracker, so
/// compression stops treating the history as frozen and rewrites it — including
/// message 0 — and the bytes no longer match the prefix the provider still
/// holds. Measured over 2,083 ledger-joined turns on 2026-08-17
/// (`bench/_wastewhere.py`): 7 turns, **352,167 tokens**, 10% of all failed
/// re-use, every one of them 0 to 193 seconds after a proxy start.
///
/// Only the forwarded bytes can reproduce the forwarded bytes. Each message was
/// compressed once, while it was the live zone, and then frozen; recompressing
/// the history from scratch compresses every message against a different context
/// and lands somewhere else. So this stores them verbatim.
///
/// # What is not stored
///
/// The `alternates` — the interleaved-stream slots. They multiply the file by the
/// number of streams sharing a session key to protect a case that is already
/// rare, and a missing alternate costs one decline, which is what happens today.
///
/// The session key itself is not written either. It "can contain an
/// authorization credential or caller-supplied identifier", which is why the
/// logs only ever print a hash of it, and the same reasoning applies harder to a
/// file that outlives the process. The file is *named* by its SHA-256, so the
/// lookup needs nothing inside.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PersistedPrefix {
    pub(super) chain_id: u64,
    pub(super) cached_token_count: u64,
    pub(super) cached_message_count: usize,
    pub(super) turn_number: u64,
    /// Unix seconds. The only staleness signal that survives a restart —
    /// `Instant` is process-local and meaningless across one.
    pub(super) saved_at_unix: u64,
    pub(super) originals: Vec<Value>,
    pub(super) forwarded: Vec<Value>,
    /// [`adoption_head_hash`] of `originals`, so a cross-session scan can skip
    /// this file without parsing its messages. Absent in files written before
    /// the field existed; those are hashed once when first scanned.
    #[serde(default)]
    pub(super) head_hash: Option<String>,
    /// [`forwarded_system_digest`] of the system block `forwarded` went out
    /// under. The adoption gate only adopts a persisted prefix under a
    /// matching post-hold system; files written before the field existed
    /// decline adoption (safe direction — one honest miss until the session
    /// turns again and rewrites the file).
    #[serde(default)]
    pub(super) forwarded_system_hash: Option<String>,
}

/// Fewest original messages a request must carry, and a donor prefix must
/// cover, before another session's forwarded prefix is adopted.
///
/// Below this the rebuild is cheap and the opening messages are mostly shared
/// scaffolding, so a match says little about the conversation. A fork
/// subagent, a resumed transcript or a restarted client all arrive well above
/// it: the pairs measured on 2026-09-02 shared 59 to 443 messages.
pub const CROSS_SESSION_ADOPT_MIN_MESSAGES: usize = 10;

/// Hash of the first two canonical messages. Two sessions whose prefixes
/// share a head are the only ones worth comparing message by message.
///
/// Canonical means [`canonicalize_for_prefix_compare`], the same projection
/// [`matches_canonical_prefix`] uses, so a `<system-reminder>` edited between
/// turns (a CLAUDE.md change) or a moved `cache_control` still finds the
/// donor. That matters because the session key itself hashes message 0 with a
/// weaker normalisation: 16 of 22 key changes measured on 2026-09-02 were a
/// reminder edit, the other 7 an OAuth token rotation, and each one is the
/// same conversation continuing under a new key.
pub(super) fn adoption_head_hash(messages: &[Value]) -> Option<String> {
    use sha2::{Digest, Sha256};
    if messages.is_empty() {
        return None;
    }
    let head = canonicalize_slice(&messages[..messages.len().min(2)]);
    let bytes = serde_json::to_vec(&head).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}

/// Digest of the `system` block a turn forwarded, for the adoption gate.
///
/// An adopted prefix replays the donor's forwarded messages under THIS turn's
/// system, so adoption is only a hit when the two systems match. `system`
/// carries `cache_control` markers that the pipeline moves every turn (tail
/// breakpoints, hold restatements), which are not cache-key material in
/// themselves — hash with every `cache_control` key stripped at any depth, so
/// marker placement never reads as a system change while real text always
/// does. Both the recorded (donor) and compared (current) sides hash the
/// post-hold, post-preview system through this one function, so the two are
/// comparable by construction. Absent system digests to the digest of null,
/// which only ever equals another absent system.
pub fn forwarded_system_digest(system: Option<&Value>) -> String {
    use sha2::{Digest, Sha256};
    fn stripped(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (key, val) in map {
                    if key == "cache_control" {
                        continue;
                    }
                    out.insert(key.clone(), stripped(val));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(stripped).collect()),
            other => other.clone(),
        }
    }
    let canonical = system.map(stripped).unwrap_or(Value::Null);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

/// Which session a prefix was adopted from, for seeding whatever other
/// per-session state has to travel with it (the offload gate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptionDonor {
    /// The donor's tracker was in memory, so its key is known.
    Session(String),
    /// The donor was found on disk, where only the SHA-256 hex of its key is
    /// known — the file name. Sibling per-session stores name their files the
    /// same way.
    PersistedDigest(String),
}

/// Called once per adoption with the donor and the adopting session key.
pub type AdoptionHook = Arc<dyn Fn(&AdoptionDonor, &str) + Send + Sync>;

/// Head hash of each persisted prefix file, by path, with the modification
/// time it was read at.
pub(super) type PersistedHeadCache =
    std::collections::HashMap<std::path::PathBuf, (Option<std::time::SystemTime>, Option<String>)>;

/// A donor prefix chosen for adoption.
pub(super) struct AdoptedPrefix {
    pub(super) originals: Vec<Value>,
    pub(super) forwarded: Vec<Value>,
    /// [`forwarded_system_digest`] of the system block `forwarded` went out
    /// under. The adoption gate compares it against the adopting turn's
    /// post-hold system: equal means the provider holds this exact prefix
    /// lineage, anything else declines.
    pub(super) forwarded_system_hash: String,
    pub(super) cached_token_count: u64,
    pub(super) cached_message_count: usize,
    pub(super) turn_number: u64,
    pub(super) donor: AdoptionDonor,
    /// Time since the donor's last turn. Breaks ties between equally long
    /// matches in favour of the donor that forwarded most recently.
    pub(super) age: Duration,
}

impl AdoptedPrefix {
    /// Longest match wins; the most recent breaks a tie.
    pub(super) fn beats(&self, other: Option<&AdoptedPrefix>) -> bool {
        match other {
            None => true,
            Some(o) => {
                self.originals.len() > o.originals.len()
                    || (self.originals.len() == o.originals.len() && self.age < o.age)
            }
        }
    }
}

/// How many leading messages of a held prefix this turn can adopt, or `None`
/// when too few to be worth it.
///
/// The donor usually went on past the fork: a subagent forks at turn 4 of a
/// session now on turn 6, so the whole held prefix does not lead the turn but
/// its first turns do. Originals and forwarded messages pair one to one (every
/// one of 340 persisted prefixes checked on 2026-09-02 had equal counts), so
/// the forwarded slice cuts at the same index.
pub(super) fn adoptable_len(
    originals: &[Value],
    forwarded: &[Value],
    canonical_current: &[Value],
) -> Option<usize> {
    if forwarded.is_empty() || originals.len() != forwarded.len() {
        return None;
    }
    let agreed = canonical_agreement_len(originals, canonical_current);
    (agreed >= CROSS_SESSION_ADOPT_MIN_MESSAGES).then_some(agreed)
}

/// Where one session's prefix lives.
///
/// Named by the SHA-256 of the session key, which keeps a possible credential out
/// of a filename and needs nothing stored inside the file to look up.
pub(super) fn persisted_path(dir: &std::path::Path, session_key: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_key.as_bytes());
    dir.join(format!("{}.json", hex::encode(digest)))
}

/// Read a session's persisted prefix, or `None` if there is none, it is
/// unreadable, or its last turn is older than [`PERSIST_MAX_AGE`].
pub(super) fn read_persisted_prefix(
    dir: &std::path::Path,
    session_key: &str,
) -> Option<PersistedPrefix> {
    read_persisted_prefix_at(&persisted_path(dir, session_key))
}

pub(super) fn read_persisted_prefix_at(path: &std::path::Path) -> Option<PersistedPrefix> {
    let bytes = std::fs::read(path).ok()?;
    let snapshot: PersistedPrefix = serde_json::from_slice(&bytes).ok()?;
    if snapshot.forwarded.is_empty() {
        return None;
    }
    (unix_now().saturating_sub(snapshot.saved_at_unix) <= PERSIST_MAX_AGE.as_secs())
        .then_some(snapshot)
}

/// Delete persisted prefixes whose last turn is past [`PERSIST_MAX_AGE`],
/// returning how many went. Runs once per process start, on one `read_dir`.
pub(super) fn sweep_stale_prefixes(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Age from the file's own mtime, not its contents: a truncated or
        // foreign file should be swept too, and parsing every one to find out
        // defeats the point of a cheap sweep.
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e > PERSIST_MAX_AGE).unwrap_or(false))
            .unwrap_or(false);
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How stale a persisted prefix may be and still be worth loading.
///
/// The proxy asks for the 1-hour tier, and a cache read refreshes the entry for
/// free — so an entry survives indefinitely while a conversation is active, and
/// dies an hour after its last turn. A snapshot whose last turn is older than
/// that names a prefix the provider has already dropped.
///
/// Being generous here is safe rather than risky: a stale prefix cannot cause a
/// wrong replay, because [`matches_canonical_prefix`] still has to accept it. The
/// worst case is the decline that would have happened anyway.
pub(super) const PERSIST_MAX_AGE: Duration = Duration::from_secs(3600);

/// Runaway guard on one session's file, not a policy choice.
///
/// A deep conversation is around 1.2 MB of messages, so originals plus forwarded
/// runs to a few MB — worth writing, since deep conversations are exactly where
/// the tokens are. This only stops something pathological from filling the disk.
pub(super) const PERSIST_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Distinguishes concurrent temporary files within one process; see
/// [`SessionReplayStore::persist`].
pub(super) static PERSIST_TMP_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How long a tracker survives with no turn on it. Past this a turn reports
/// [`PrefixMiss::IdlePastTtl`] and rebuilds instead of replaying.
///
/// # Why one hour, measured 2026-08-17
///
/// This was 600 seconds, which is the 5-minute cache tier plus margin. The proxy
/// forces a **one hour** provider TTL (`--force-1h-cache-ttl`), and reads renew an
/// entry for free, so between ten and sixty minutes the provider still held a
/// prefix that this store had already thrown away. Losing the tracker is not a
/// cheap decline: `frozen_message_count` returns 0 without one, so compression
/// rewrites the conversation from message 0 and the provider matches nothing.
///
/// Two turns in a 142-turn window paid for that — 247 messages deep at 433,366
/// tokens of creation and 283 deep at 255,527, against roughly 1,000 for a turn
/// of that depth that replays. Together they were 99% of everything the proxy
/// billed above what an unproxied client would have on that window.
///
/// An earlier measurement priced idle declines at 750 tokens against a 1,648
/// baseline and concluded this constant was harmless. That sample was 14 shallow
/// turns; the cost is entirely in deep conversations, so it missed them.
///
/// Being wrong in the other direction is cheap: if the provider HAS dropped the
/// entry, replaying a prefix it no longer holds costs a rewrite, which is exactly
/// what declining costs. Correctness does not rest on this number either — a
/// replayed prefix still has to pass `matches_canonical_prefix`.
///
/// Kept in step with [`PERSIST_MAX_AGE`], which bounds the same staleness on disk.
pub(super) const SESSION_TTL: Duration = Duration::from_secs(3600);

/// Per-session freeze-replay store. Cloneable `Arc<Mutex<…>>` handle like
/// [`crate::cache_stabilization::drift_detector::DriftState`].
#[derive(Clone)]
pub struct SessionReplayStore {
    pub(super) trackers: Arc<Mutex<LruCache<String, PrefixReplayTracker>>>,
    pub(super) pending: Arc<Mutex<LruCache<String, PendingTurn>>>,
    /// How long a tracker survives without a turn. See [`SESSION_TTL`].
    pub(super) session_ttl: Duration,
    /// Where prefixes are persisted, or `None` to keep everything in memory.
    pub(super) persist_dir: Option<Arc<std::path::PathBuf>>,
    /// [`adoption_head_hash`] of every in-memory prefix to the session keys
    /// holding one, so a cross-session scan touches only candidates. Keys
    /// whose tracker the LRU has since evicted are skipped when looked up.
    pub(super) head_index: Arc<Mutex<std::collections::HashMap<String, Vec<String>>>>,
    /// Head hash of each persisted file, keyed by path and remembered with the
    /// modification time it was read at, so a scan parses each file once.
    pub(super) persisted_heads: Arc<Mutex<PersistedHeadCache>>,
    pub(super) adoption_hook: Option<AdoptionHook>,
    /// Held from snapshot to rename in [`Self::complete`], so two turns of one
    /// session completing at once cannot write their files in the wrong order
    /// and leave the older snapshot on disk.
    pub(super) persist_lock: Arc<Mutex<()>>,
    /// Adopter session key → donor hash, for the turn that adopted. Read once
    /// by [`SessionReplayStore::take_adoption`] so the usage observer can name
    /// the donor on that turn's first-turn event.
    pub(super) recent_adoptions: Arc<Mutex<LruCache<String, String>>>,
}

impl std::fmt::Debug for SessionReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReplayStore")
            .field("capacity", &REPLAY_STORE_CAPACITY)
            .finish_non_exhaustive()
    }
}

/// Pick the stored prefix leading this turn: first an exact canonical
/// prefix (longest wins), then a tail-edited continuation (longest
/// agreeing run wins, within slack). Returns the chain id plus the
/// original/forwarded pair. `system_ok` gates candidates on the system
/// they went out under.
/// Extracted from `previous_turn_for` without behavior change.
pub(super) fn pick_replay_candidate<'t>(
    tracker: &'t PrefixReplayTracker,
    canonical_current: &[Value],
    system_ok: &dyn Fn(&str) -> bool,
) -> Option<(u64, &'t Vec<Value>, &'t Vec<Value>)> {
    let best = std::iter::once((
        tracker.primary_chain_id,
        &tracker.last_original_messages,
        &tracker.last_forwarded_messages,
        tracker.last_forwarded_system_hash.as_str(),
    ))
    .chain(
        tracker
            .alternates
            .iter()
            .map(|(id, o, f, s)| (*id, o, f, s.as_str())),
    )
    .filter(|(_, o, f, s)| {
        !f.is_empty() && matches_canonical_prefix(o, canonical_current) && system_ok(s)
    })
    .max_by_key(|(_, o, _, _)| o.len())
    .map(|(id, o, f, _)| (id, o, f));
    // Nothing leads this turn exactly. Before giving up on identity,
    // look for a stream this turn continues with its tail edited —
    // the client rewriting a message it already sent, which is what
    // a content divergence is. The overlay can replay everything
    // ahead of the edit, but only if it is told whose prefix this
    // is, so the answer has to carry that stream's real chain id
    // rather than the fallback's zero.
    best.or_else(|| {
        std::iter::once((
            tracker.primary_chain_id,
            &tracker.last_original_messages,
            &tracker.last_forwarded_messages,
            tracker.last_forwarded_system_hash.as_str(),
        ))
        .chain(
            tracker
                .alternates
                .iter()
                .map(|(id, o, f, s)| (*id, o, f, s.as_str())),
        )
        .filter_map(|(id, o, f, s)| {
            if f.is_empty() {
                return None;
            }
            let agreed = canonical_agreement_len(o, canonical_current);
            (agreed >= MIN_AGREEING_RUN && agreed + TAIL_EDIT_SLACK >= o.len() && system_ok(s))
                .then_some((agreed, id, o, f))
        })
        .max_by_key(|(agreed, ..)| *agreed)
        .map(|(_, id, o, f)| (id, o, f))
    })
}

/// Did a stream other than the session's most recent one win? That is
/// this store's whole reason to exist, and the only direct evidence that
/// interleaved streams were costing busts: under one slot per session
/// this turn would have declined and forwarded fresh bytes over cached
/// content.
/// Extracted from `previous_turn_for` without behavior change.
pub(super) fn note_matched_alternate(
    tracker: &PrefixReplayTracker,
    o: &Vec<Value>,
    chain_id: u64,
    current_msgs: usize,
) {
    let primary_len = tracker.last_original_messages.len();
    let matched_alternate = o.len() != primary_len || o != &tracker.last_original_messages;
    if matched_alternate {
        tracing::info!(
            event = "prefix_replay_matched_alternate",
            alternates_held = tracker.alternates.len(),
            matched_prefix_msgs = o.len(),
            most_recent_prefix_msgs = primary_len,
            current_msgs = current_msgs,
            chain_id = chain_id,
            "replayed a stream's own prefix instead of the session's \
             most recent one; one slot per session would have declined here"
        );
    }
}

impl SessionReplayStore {
    /// Build a store bounded to `capacity` sessions. Production uses
    /// [`REPLAY_STORE_CAPACITY`]; tests pass small values.
    ///
    /// # Panics
    /// If `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("SessionReplayStore capacity must be > 0");
        let pending_cap =
            NonZeroUsize::new(PENDING_CAPACITY).expect("PENDING_CAPACITY must be > 0");
        Self {
            trackers: Arc::new(Mutex::new(LruCache::new(cap))),
            pending: Arc::new(Mutex::new(LruCache::new(pending_cap))),
            session_ttl: SESSION_TTL,
            persist_dir: None,
            head_index: Arc::new(Mutex::new(std::collections::HashMap::new())),
            persisted_heads: Arc::new(Mutex::new(std::collections::HashMap::new())),
            adoption_hook: None,
            persist_lock: Arc::new(Mutex::new(())),
            recent_adoptions: Arc::new(Mutex::new(LruCache::new(pending_cap))),
        }
    }

    /// The donor hash if `session_key` adopted another session's prefix and
    /// nobody has read that outcome yet. Consumes it.
    pub fn take_adoption(&self, session_key: &str) -> Option<String> {
        self.recent_adoptions.lock().ok()?.pop(session_key)
    }

    /// Seconds since this session's last recorded turn, if it has a tracker.
    /// Backs the cold-prefix gate: provider caches lapse on idle time, so a
    /// lane idle past TTL holds a dead prefix. `None` for unknown sessions
    /// (first turn) — callers must treat that as warm, never cold: there is
    /// no baseline to compare against, and an unknown is not evidence.
    pub fn idle_seconds(&self, session_key: &str) -> Option<f64> {
        let guard = self.trackers.lock().ok()?;
        guard
            .peek(session_key)
            .map(|t| t.last_activity.elapsed().as_secs_f64())
    }

    /// Run `hook` whenever a session adopts another session's prefix.
    pub fn set_adoption_hook(&mut self, hook: AdoptionHook) {
        self.adoption_hook = Some(hook);
    }

    /// [`Self::new`], with prefixes persisted under `dir` so they survive a
    /// restart. See [`PersistedPrefix`] for what is written and why.
    ///
    /// Creates the directory and sweeps anything past [`PERSIST_MAX_AGE`] out of
    /// it. A failure to do either turns persistence off for this process rather
    /// than failing the proxy: the feature is a cost optimisation, and the
    /// in-memory path it falls back to is the behaviour that shipped for months.
    ///
    /// # Panics
    /// If `capacity == 0`.
    pub fn with_persistence(capacity: usize, dir: std::path::PathBuf) -> Self {
        let mut store = Self::new(capacity);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                event = "prefix_replay_persist_unavailable",
                dir = %dir.display(),
                %error,
                "cannot create the prefix-replay directory; keeping prefixes in memory only"
            );
            return store;
        }
        let swept = sweep_stale_prefixes(&dir);
        tracing::info!(
            event = "prefix_replay_persist_enabled",
            dir = %dir.display(),
            stale_files_removed = swept,
            "persisting forwarded prefixes across restarts"
        );
        store.persist_dir = Some(Arc::new(dir));
        store
    }

    /// Load this session's persisted prefix into memory, if there is one and the
    /// store does not already hold it.
    ///
    /// The file read happens with no lock held. Taking the lock twice — once to
    /// see whether the read is needed, once to install the result — costs two
    /// uncontended acquisitions and keeps disk I/O out of the path every other
    /// request takes through [`Self::previous_turn_for`].
    pub(super) fn hydrate(&self, session_key: &str) {
        let Some(dir) = self.persist_dir.as_deref() else {
            return;
        };
        match self.trackers.lock() {
            Ok(guard) if guard.contains(session_key) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let Some(snapshot) = read_persisted_prefix(dir, session_key) else {
            return;
        };
        let messages = snapshot.forwarded.len();
        let tracker = PrefixReplayTracker {
            cached_token_count: snapshot.cached_token_count,
            cached_message_count: snapshot.cached_message_count,
            turn_number: snapshot.turn_number,
            // Process-local, so it cannot be restored — and must not read as
            // stale, or the session TTL below would drop what we just loaded.
            // `saved_at_unix` is what actually bounds the age, checked on read.
            last_activity: Instant::now(),
            last_original_messages: snapshot.originals,
            last_forwarded_messages: snapshot.forwarded,
            // Files written before the field existed rehydrate without it and
            // decline adoption and replay until the next served turn records
            // the real digest — one honest miss, same contract as the gate.
            last_forwarded_system_hash: snapshot.forwarded_system_hash.unwrap_or_default(),
            alternates: Vec::new(),
            primary_chain_id: snapshot.chain_id,
            next_chain_id: snapshot.chain_id.saturating_add(1),
        };
        let head = adoption_head_hash(&tracker.last_original_messages);
        if let Ok(mut guard) = self.trackers.lock()
            && !guard.contains(session_key)
        {
            let evicted = guard.push(session_key.to_string(), tracker);
            self.unindex_evicted(session_key, evicted);
            self.index_head(session_key, head);
            tracing::info!(
                event = "prefix_replay_rehydrated",
                session_key_hash =
                    %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
                prefix_msgs = messages,
                chain_id = snapshot.chain_id,
                "restored a forwarded prefix written before this process started"
            );
        }
    }

    /// Write this session's primary chain to disk. Best-effort and quiet on
    /// failure — a lost snapshot costs one decline after the next restart.
    pub(super) fn persist(&self, session_key: &str, snapshot: PersistedPrefix) {
        let Some(dir) = self.persist_dir.as_deref() else {
            return;
        };
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) if bytes.len() <= PERSIST_MAX_BYTES => bytes,
            Ok(bytes) => {
                tracing::warn!(
                    event = "prefix_replay_persist_skipped",
                    bytes = bytes.len(),
                    limit = PERSIST_MAX_BYTES,
                    "prefix is larger than the runaway guard; not persisting it"
                );
                return;
            }
            Err(_) => return,
        };
        let path = persisted_path(dir, session_key);
        // Write-then-rename, so a restart mid-write cannot leave a truncated
        // file that reads as a valid but wrong prefix. The temporary name is
        // unique per process and write, so two writers of one session (two
        // proxies on one store dir, or two in-flight turns) cannot rename
        // each other's half-written file into place.
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            PERSIST_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if std::fs::write(&temporary, &bytes).is_ok() && std::fs::rename(&temporary, &path).is_ok()
        {
            // Remember the head of what was just written. Without this the next
            // adoption scan sees a file whose mtime moved, and reads and parses
            // it back purely to learn a hash this side already had — and every
            // session's turn rewrites its own file, so with many sessions open
            // each scan re-parsed most of the store. Measured at 41 files and
            // 16.4 MB: 77 ms of parsing per full pass, on the request path.
            let head = snapshot
                .head_hash
                .clone()
                .or_else(|| adoption_head_hash(&snapshot.originals));
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            if let Ok(mut heads) = self.persisted_heads.lock() {
                heads.insert(path.clone(), (modified, head));
            }
        }
    }

    /// Snapshot of the previous turn's `(original, forwarded)` messages for a
    /// session, or `None` if there is no live prefix to replay (cold start, or
    /// idle beyond the session TTL). Used to build the overlay inputs.
    pub fn previous_turn(&self, session_key: &str) -> Option<(Vec<Value>, Vec<Value>)> {
        self.previous_turn_detailed(session_key).ok()
    }

    /// The stored prefix that `current_originals` actually continues.
    ///
    /// One session key carries several streams (item 11), so "the last turn on
    /// this session" is the wrong question — it may belong to a different
    /// stream, in which case the append-only guard rejects it and the turn
    /// forwards fresh bytes over content the provider had cached. This asks the
    /// right question instead: of the prefixes held for this session, which one
    /// does this turn extend? Longest match wins, so a stream replays as much
    /// of its own history as it can.
    ///
    /// Falls back to the most recent prefix when nothing matches, leaving the
    /// overlay to decline exactly as before — this can only turn a decline into
    /// a replay, never a replay into a wrong one.
    ///
    /// The third element is the id of the chain this turn continues, or `0`
    /// when it continues none of them (the fallback below). That is the only
    /// trustworthy grouping key the proxy has — see [`PrefixReplayTracker`].
    pub fn previous_turn_for(
        &self,
        session_key: &str,
        current_originals: &[Value],
        current_system_hash: Option<&str>,
    ) -> Result<(Vec<Value>, Vec<Value>, u64), PrefixMiss> {
        // A restart empties this store, and the miss that follows is expensive
        // rather than free — see [`PersistedPrefix`]. No-op unless persistence is
        // configured or the session is already in memory.
        //
        // `current_system_hash` is the [`forwarded_system_digest`] of the
        // system block this turn will forward (post-hold). Replaying stored
        // messages is only a hit under the system they went out under, so a
        // candidate whose recorded system differs is not a replay source —
        // not a mismatch to splice around, a different cache lineage
        // entirely. `None` (callers without the system in hand, and tests)
        // skips the precondition and behaves exactly as before.
        let canonical_current = canonicalize_slice(current_originals);
        let declined_on_system = self.hydrate_or_adopt(
            session_key,
            current_originals,
            &canonical_current,
            current_system_hash,
        );
        let mut guard = match self.trackers.lock() {
            Ok(g) => g,
            Err(_) => return Err(PrefixMiss::LockPoisoned),
        };
        let Some(tracker) = guard.get(session_key) else {
            // A donor matched by messages but declined on system leaves no
            // tracker behind: the lane is genuinely new, and the lineage
            // event above already named why. Reporting the cold-start miss
            // here would bury that cause.
            return Err(if declined_on_system {
                PrefixMiss::SystemChanged
            } else {
                PrefixMiss::NoTrackerForSession
            });
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            if let Some(idle) = guard.pop(session_key) {
                self.unindex_head(session_key, &idle);
            }
            return Err(PrefixMiss::IdlePastTtl);
        }
        if tracker.last_forwarded_messages.is_empty() && tracker.alternates.is_empty() {
            return Err(PrefixMiss::NothingForwardedYet);
        }
        // System precondition shared by both scans below: a candidate replays
        // only under the system it went out under (see `current_system_hash`
        // above). `None` callers pass a skip that keeps yesterday's behavior
        // bit for bit; `Some` turns a foreign-system match from a splice
        // into a decline.
        let system_ok = |stored_hash: &str| {
            current_system_hash
                .map(|current| stored_hash == current)
                .unwrap_or(true)
        };
        let best = pick_replay_candidate(tracker, &canonical_current, &system_ok);
        match best {
            Some((chain_id, o, f)) => {
                note_matched_alternate(tracker, o, chain_id, current_originals.len());
                Ok((o.clone(), f.clone(), chain_id))
            }
            None if tracker.last_forwarded_messages.is_empty() => {
                Err(PrefixMiss::NothingForwardedYet)
            }
            // Held messages lead this turn, but under a different system.
            // Replaying them would splice stored bytes under a system no
            // provider cache holds — a miss that reports as a replay — so
            // the turn forwards its own bytes instead. Same provider
            // outcome as a clean miss, honestly measured: `miss_detail`
            // carries `system_changed` and this event names the lineage.
            // Only with a system in hand; `None` callers keep the arm below
            // exactly as before.
            None if current_system_hash.is_some()
                && pick_replay_candidate(tracker, &canonical_current, &|_: &str| true)
                    .is_some() =>
            {
                tracing::info!(
                    event = "prefix_replay_system_changed",
                    session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
                    current_msgs = current_originals.len(),
                    stored_prefix_msgs = tracker.last_original_messages.len(),
                    alternates_held = tracker.alternates.len(),
                    "held messages lead this turn under a different system; \
                     forwarding fresh bytes instead of replaying across systems"
                );
                Err(PrefixMiss::SystemChanged)
            }
            // Nothing held leads this turn. The prefix goes back for the
            // overlay to report against, but the chain id is 0: this turn
            // continues none of them, and saying otherwise would put two
            // unrelated runs of turns under one name.
            None => {
                // This arm is where the money goes. Measured over the
                // 2026-08-20/22 logs it took 51 turns — 0.36% of traffic — and
                // 2.59M tokens of cache creation, 5.6% of the total, at ~50k
                // per turn. Every one of them re-cached a large prefix because
                // the store held nothing that led the turn.
                //
                // Three things produce that and they need opposite answers: a
                // genuinely new stream (correct, nothing to do), a stream whose
                // entry was evicted under the alternates caps (raise a cap), and
                // one that agreed for a long run and then broke (a real
                // divergence). Only the longest agreeing run tells them apart,
                // so it is computed here, on a path that already declined.
                let best_agreement = std::iter::once(&tracker.last_original_messages)
                    .chain(tracker.alternates.iter().map(|(_, o, _, _)| o))
                    .map(|o| canonical_agreement_len(o, &canonical_current))
                    .max()
                    .unwrap_or(0);
                tracing::info!(
                    event = "prefix_replay_no_stream_leads_turn",
                    alternates_held = tracker.alternates.len(),
                    held_messages = tracker
                        .alternates
                        .iter()
                        .map(|(_, o, _, _)| o.len())
                        .sum::<usize>(),
                    primary_prefix_msgs = tracker.last_original_messages.len(),
                    current_msgs = current_originals.len(),
                    // 0 means no held stream shares even an opening with this
                    // turn, which is what an eviction or a brand-new stream
                    // looks like. A long run means identity was nearly there.
                    best_agreement_msgs = best_agreement,
                    max_alternate_prefixes = MAX_ALTERNATE_PREFIXES,
                    max_alternate_messages = MAX_ALTERNATE_MESSAGES,
                    "no held stream leads this turn; replaying nothing and \
                     re-caching the prefix"
                );
                Ok((
                    tracker.last_original_messages.clone(),
                    tracker.last_forwarded_messages.clone(),
                    0,
                ))
            }
        }
    }

    /// [`Self::previous_turn`], but saying why there was nothing to replay.
    ///
    /// `no_previous_turn` is the commonest reason a turn declines to replay,
    /// and on its own it is not actionable: a genuine first turn is free, a
    /// session key that changed between turns is a bug worth chasing, and an
    /// idle gap past the TTL is neither — the provider's cache (5 minutes) died
    /// long before this store's 10. They need different responses, so they get
    /// different names.
    pub fn previous_turn_detailed(
        &self,
        session_key: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), PrefixMiss> {
        self.hydrate(session_key);
        let mut guard = match self.trackers.lock() {
            Ok(g) => g,
            Err(_) => return Err(PrefixMiss::LockPoisoned),
        };
        let Some(tracker) = guard.get(session_key) else {
            return Err(PrefixMiss::NoTrackerForSession);
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            if let Some(idle) = guard.pop(session_key) {
                self.unindex_head(session_key, &idle);
            }
            return Err(PrefixMiss::IdlePastTtl);
        }
        if tracker.last_forwarded_messages.is_empty() {
            return Err(PrefixMiss::NothingForwardedYet);
        }
        Ok((
            tracker.last_original_messages.clone(),
            tracker.last_forwarded_messages.clone(),
        ))
    }

    /// Whether this turn's history goes to the provider as a fresh write.
    ///
    /// True when nothing is stored for the session, the stored turn is older
    /// than the session TTL, or the stored turn is not a prefix of what the
    /// client sent now (a resume, a compaction, a rewound branch). In each of
    /// those cases the provider has nothing to read back, so rewriting the
    /// history with tool results offloaded costs no cache entry that a
    /// verbatim copy would have kept. False when the stored prefix replays,
    /// where the same rewrite would forfeit a warm read.
    pub fn history_will_be_rewritten(
        &self,
        session_key: &str,
        incoming_original: &[Value],
    ) -> bool {
        let canonical_incoming = canonicalize_slice(incoming_original);
        // Ungated (`None`): the probe has no post-hold system yet, and it must
        // answer exactly as it always has — installing on message agreement so
        // a turn that will replay reports fresh=false. The real path
        // re-decides with the system in hand; a gate decline there only ever
        // turns an adopted replay into an honest miss (offload skipped,
        // verbatim bytes), never a replay into a wrong one.
        self.hydrate_or_adopt(session_key, incoming_original, &canonical_incoming, None);
        let Ok(guard) = self.trackers.lock() else {
            return false;
        };
        let Some(tracker) = guard.peek(session_key) else {
            return true;
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            return true;
        }
        if tracker.last_forwarded_messages.is_empty() {
            return false;
        }
        let replayable =
            matches_canonical_prefix(&tracker.last_original_messages, &canonical_incoming)
                || tracker.alternates.iter().any(|(_, original, _, _)| {
                    matches_canonical_prefix(original, &canonical_incoming)
                });
        !replayable
    }

    /// How many leading messages this turn still shares with anything the
    /// session has stored, counting the primary prefix and every alternate.
    ///
    /// Only meaningful once `history_will_be_rewritten` has said yes: it tells
    /// a caller about to rewrite the history how much of it the provider may
    /// still be holding. `None` when there is no tracker to compare against,
    /// which is the case that has nothing cached to lose.
    ///
    /// Careful with the answer. The run agrees on what the *client* sent, and
    /// the provider cached what the proxy *forwarded*, which is not the same
    /// bytes on any session where a rewriting pass has already run. Treat it as
    /// an upper bound on what a rewrite could cost, not as a licence to skip it.
    pub fn agreed_prefix_len(
        &self,
        session_key: &str,
        incoming_original: &[Value],
    ) -> Option<usize> {
        let canonical_incoming = canonicalize_slice(incoming_original);
        self.hydrate(session_key);
        let guard = self.trackers.lock().ok()?;
        let tracker = guard.peek(session_key)?;
        if tracker.last_activity.elapsed() > self.session_ttl {
            return None;
        }
        if tracker.last_forwarded_messages.is_empty() {
            return None;
        }
        let best = std::iter::once(&tracker.last_original_messages)
            .chain(
                tracker
                    .alternates
                    .iter()
                    .map(|(_, original, _, _)| original),
            )
            .map(|candidate| canonical_agreement_len(candidate, &canonical_incoming))
            .max()
            .unwrap_or(0);
        Some(best)
    }

    /// How many leading messages this turn shares with what the proxy last
    /// *forwarded*, as opposed to what the client last sent.
    ///
    /// This is the number that decides whether rewriting the head costs
    /// anything. The provider cached the bytes we forwarded, and on any
    /// session where a stripping pass has already run those are not the bytes
    /// the client holds — so `agreed_prefix_len`, which compares originals,
    /// can say the head is intact while the provider's copy of it is not.
    /// Comparing against the forwarded copy answers the question directly.
    ///
    /// Still an upper bound: the passes that run after this one may rewrite
    /// the head again before it goes out. `None` when there is nothing
    /// forwarded to compare against, which is the case with nothing to lose.
    pub fn forwarded_agreement_len(
        &self,
        session_key: &str,
        incoming_original: &[Value],
    ) -> Option<usize> {
        let canonical_incoming = canonicalize_slice(incoming_original);
        self.hydrate(session_key);
        let guard = self.trackers.lock().ok()?;
        let tracker = guard.peek(session_key)?;
        if tracker.last_activity.elapsed() > self.session_ttl {
            return None;
        }
        if tracker.last_forwarded_messages.is_empty() {
            return None;
        }
        Some(canonical_agreement_len(
            &tracker.last_forwarded_messages,
            &canonical_incoming,
        ))
    }

    /// Turns this proxy has seen of `session_key`, or `None` when the session
    /// is unknown.
    ///
    /// Not `messages.len() / 2`: a session that arrives mid-conversation
    /// carries a long history the proxy never forwarded, and the two numbers
    /// are what tell those apart. Whether a deferred offload backlog would
    /// ever pay for itself turns on how many more turns the conversation has
    /// left, and turns already served is the only evidence available for that.
    pub fn turns_seen(&self, session_key: &str) -> Option<u64> {
        self.hydrate(session_key);
        let guard = self.trackers.lock().ok()?;
        let tracker = guard.peek(session_key)?;
        if tracker.last_activity.elapsed() > self.session_ttl {
            return None;
        }
        Some(tracker.turn_number())
    }

    /// How many of this session's messages the proxy has already sent upstream,
    /// counting the primary prefix, every held alternate, and any turn still in
    /// flight. `None` when the session is unknown or idle past its TTL.
    ///
    /// A message at or past this index has never left the proxy, so rewriting
    /// it cannot break a cache entry. Everything below it may have been cached
    /// verbatim; a caller unsure of that must leave it alone.
    pub fn forwarded_message_count(&self, session_key: &str) -> Option<usize> {
        self.hydrate(session_key);
        let tracked = {
            let guard = self.trackers.lock().ok()?;
            let tracker = guard.peek(session_key)?;
            if tracker.last_activity.elapsed() > self.session_ttl {
                return None;
            }
            tracker
                .alternates
                .iter()
                .map(|(_, original, _, _)| original.len())
                .fold(tracker.last_original_messages.len(), usize::max)
        };
        let in_flight = self
            .pending
            .lock()
            .ok()
            .map(|guard| {
                guard
                    .iter()
                    .filter(|(_, turn)| turn.session_key == session_key)
                    .map(|(_, turn)| turn.original_messages.len())
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        Some(tracked.max(in_flight))
    }

    /// How many leading messages the provider has confirmed cached for this
    /// session, or 0 when unknown (no tracker, idle past its TTL, cold cache).
    ///
    /// The overlay's unconditional-replay floor (port of upstream `aebe9895`):
    /// inside this prefix the replay source is exactly what the provider
    /// hashed, so replaying it can only help; beyond it the non-inflation
    /// bound decides. Read from the live tracker — which the response side
    /// feeds via [`Self::complete`] — rather than threaded through
    /// [`Self::previous_turn_for`], so the overlay call site needs no new
    /// plumbing; same pattern as [`Self::turns_seen`].
    ///
    /// Approximate on alternate-chain turns: the count belongs to the
    /// session's primary tracker while the replayed bytes may come from a
    /// held alternate. Either direction is safe — the floor only bypasses the
    /// size bound, never an alignment guard, so the worst case is the
    /// historical unbounded replay on one side and a conservative decline on
    /// the other.
    pub fn confirmed_frozen_count(&self, session_key: &str) -> usize {
        self.hydrate(session_key);
        let guard = match self.trackers.lock() {
            Ok(guard) => guard,
            Err(_) => return 0,
        };
        let Some(tracker) = guard.peek(session_key) else {
            return 0;
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            return 0;
        }
        tracker.frozen_message_count()
    }

    #[cfg(test)]
    pub(super) fn set_session_ttl_for_test(&mut self, ttl: Duration) {
        self.session_ttl = ttl;
    }

    /// Invalidate a session's stored prefix (drift/rebuild boundary).
    pub fn invalidate(&self, session_key: &str) {
        if let Ok(mut guard) = self.trackers.lock()
            && let Some(t) = guard.get_mut(session_key)
        {
            t.invalidate();
        }
    }

    /// Park this turn's original + forwarded messages under `request_id` so
    /// [`complete`](Self::complete) can attribute the response's cache tokens.
    /// `forwarded_system_hash` is the [`forwarded_system_digest`] of the
    /// system block the forwarded messages went out under — recorded into the
    /// tracker at completion for the adoption gate.
    pub fn begin_request(
        &self,
        request_id: &str,
        session_key: &str,
        original_messages: Vec<Value>,
        forwarded_messages: Vec<Value>,
        forwarded_system_hash: String,
    ) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.put(
                request_id.to_string(),
                PendingTurn {
                    session_key: session_key.to_string(),
                    original_messages,
                    forwarded_messages,
                    forwarded_system_hash,
                },
            );
        }
    }

    /// Feed the completed turn's cache tokens back into the session tracker.
    /// Called from the response side once the stream completes cleanly. No-op
    /// when the request was never parked (e.g. non-Anthropic, or feature off).
    pub fn complete(&self, request_id: &str, cache_read_tokens: u64, cache_write_tokens: u64) {
        let pending = match self.pending.lock() {
            Ok(mut g) => g.pop(request_id),
            Err(_) => None,
        };
        let Some(pending) = pending else {
            return;
        };
        // Taken before the tracker lock and released after the rename, which
        // orders whole snapshot-and-write sequences rather than just writes.
        // Disk I/O under it stalls only other completions, not the request
        // path, which never takes it.
        let _serialised = self.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
        // Taken under the lock, written outside it. Serializing a few MB while
        // holding the trackers mutex would stall every other request's
        // `previous_turn_for` for the length of a disk write.
        let mut snapshot = None;
        if let Ok(mut guard) = self.trackers.lock() {
            let tracker = if let Some(t) = guard.get_mut(&pending.session_key) {
                t
            } else {
                let evicted =
                    guard.push(pending.session_key.clone(), PrefixReplayTracker::default());
                self.unindex_evicted(&pending.session_key, evicted);
                guard.get_mut(&pending.session_key).unwrap()
            };
            tracker.update_from_response(
                cache_read_tokens,
                cache_write_tokens,
                &pending.forwarded_messages,
                Some(&pending.original_messages),
                pending.forwarded_system_hash,
            );
            let head_hash = adoption_head_hash(&tracker.last_original_messages);
            if self.persist_dir.is_some() && !tracker.last_forwarded_messages.is_empty() {
                snapshot = Some(PersistedPrefix {
                    chain_id: tracker.primary_chain_id,
                    cached_token_count: tracker.cached_token_count,
                    cached_message_count: tracker.cached_message_count,
                    turn_number: tracker.turn_number,
                    saved_at_unix: unix_now(),
                    originals: tracker.last_original_messages.clone(),
                    forwarded: tracker.last_forwarded_messages.clone(),
                    head_hash: head_hash.clone(),
                    forwarded_system_hash: Some(tracker.last_forwarded_system_hash.clone()),
                });
            }
            self.index_head(&pending.session_key, head_hash);
        }
        if let Some(snapshot) = snapshot {
            self.persist(&pending.session_key, snapshot);
        }
    }

    pub(super) fn index_head(&self, session_key: &str, head: Option<String>) {
        let Some(head) = head else {
            return;
        };
        if let Ok(mut index) = self.head_index.lock() {
            let keys = index.entry(head).or_default();
            if !keys.iter().any(|k| k == session_key) {
                keys.push(session_key.to_string());
            }
        }
    }

    /// Forget a tracker's head, so the index holds only keys the LRU still
    /// does. Called wherever a tracker leaves it: LRU eviction and TTL expiry.
    pub(super) fn unindex_head(&self, session_key: &str, tracker: &PrefixReplayTracker) {
        let Some(head) = adoption_head_hash(&tracker.last_original_messages) else {
            return;
        };
        if let Ok(mut index) = self.head_index.lock()
            && let Some(keys) = index.get_mut(&head)
        {
            keys.retain(|k| k != session_key);
            if keys.is_empty() {
                index.remove(&head);
            }
        }
    }

    /// What `LruCache::push` returned: the tracker it evicted to make room, or
    /// the previous value under the same key, which is not an eviction.
    pub(super) fn unindex_evicted(
        &self,
        session_key: &str,
        evicted: Option<(String, PrefixReplayTracker)>,
    ) {
        if let Some((key, tracker)) = evicted
            && key != session_key
        {
            self.unindex_head(&key, &tracker);
        }
    }

    /// Whether some prefix held for `session_key` leads `canonical_current`.
    /// `false` when there is no tracker, it idled past the TTL, or nothing it
    /// holds is a prefix of this turn.
    pub(super) fn leads_turn(&self, session_key: &str, canonical_current: &[Value]) -> bool {
        let Ok(guard) = self.trackers.lock() else {
            // Never adopt on a poisoned lock; the caller reports the miss.
            return true;
        };
        let Some(tracker) = guard.peek(session_key) else {
            return false;
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            return false;
        }
        std::iter::once((
            &tracker.last_original_messages,
            &tracker.last_forwarded_messages,
        ))
        .chain(tracker.alternates.iter().map(|(_, o, f, _)| (o, f)))
        .any(|(o, f)| !f.is_empty() && matches_canonical_prefix(o, canonical_current))
    }

    /// Restore this session's own prefix; failing that, on a turn that arrives
    /// with real history, adopt another session's.
    ///
    /// A fork subagent, a resumed transcript and a restarted client all send
    /// hundreds of messages the provider has already cached — under the
    /// session key that forwarded them, not this one. Rebuilding forwards
    /// different bytes (offload digests, markers and injections land
    /// elsewhere), so the provider reads only the shared `system`/`tools` head
    /// and writes the whole conversation again. Replaying the donor's bytes
    /// instead makes this turn read what the donor wrote.
    ///
    /// The donor is not touched. The adopter gets a copy of the matched slice
    /// as its own prefix and carries it forward from there.
    ///
    /// Returns whether a donor matched by messages but declined on system:
    /// the caller reports that as [`PrefixMiss::SystemChanged`] rather than
    /// a cold-start miss, so the decline is attributable instead of silent.
    pub(super) fn hydrate_or_adopt(
        &self,
        session_key: &str,
        current_originals: &[Value],
        canonical_current: &[Value],
        current_system_hash: Option<&str>,
    ) -> bool {
        self.hydrate(session_key);
        if current_originals.len() < CROSS_SESSION_ADOPT_MIN_MESSAGES
            || self.leads_turn(session_key, canonical_current)
        {
            return false;
        }
        let Some(head) = adoption_head_hash(current_originals) else {
            return false;
        };
        let adopted = self
            .adoption_candidate_in_memory(session_key, &head, canonical_current)
            .or_else(|| self.adoption_candidate_on_disk(session_key, &head, canonical_current));
        let Some(adopted) = adopted else {
            return false;
        };
        // The adoption gate: an adopted prefix replays the donor's forwarded
        // messages under THIS turn's system, so the adoption is only a hit
        // when the two systems match. A new lane implies a different system
        // by construction (the lane key folds the system digest in), which is
        // exactly when message agreement alone lies: the bytes are right but
        // no provider cache holds them, so installing would both miss and
        // misfile the turn as replayed. `None` (the freshness probe, which has
        // no post-hold system yet) skips the gate — the probe only decides
        // offload, and the real path re-decides with the system in hand.
        if let Some(current) = current_system_hash
            && adopted.forwarded_system_hash != current
        {
            let (donor_hash, source) = match &adopted.donor {
                AdoptionDonor::Session(key) => (
                    crate::cache_stabilization::drift_detector::session_key_log_prefix(key),
                    "memory",
                ),
                AdoptionDonor::PersistedDigest(digest) => {
                    (digest.chars().take(16).collect::<String>(), "disk")
                }
            };
            tracing::info!(
                event = "prefix_adoption_declined_system_mismatch",
                session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
                donor_session_key_hash = %donor_hash,
                source = source,
                adopted_msgs = adopted.originals.len(),
                incoming_msgs = current_originals.len(),
                "a session sharing this turn's history carries a different system; \
                 forwarding fresh bytes instead of replaying under a system no cache holds"
            );
            return true;
        }
        let (donor_hash, source) = match &adopted.donor {
            AdoptionDonor::Session(key) => (
                crate::cache_stabilization::drift_detector::session_key_log_prefix(key),
                "memory",
            ),
            AdoptionDonor::PersistedDigest(digest) => {
                (digest.chars().take(16).collect::<String>(), "disk")
            }
        };
        tracing::info!(
            event = "prefix_adopted_from_session",
            session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            donor_session_key_hash = %donor_hash,
            source = source,
            adopted_msgs = adopted.originals.len(),
            incoming_msgs = current_originals.len(),
            "a session arriving with history matched another session's prefix; \
             forwarding the bytes that session forwarded"
        );
        let donor = adopted.donor.clone();
        self.install_adopted(session_key, adopted);
        if let Ok(mut recent) = self.recent_adoptions.lock() {
            recent.put(session_key.to_string(), donor_hash);
        }
        if let Some(hook) = self.adoption_hook.as_ref() {
            hook(&donor, session_key);
        }
        false
    }

    /// The longest prefix held in memory by another live session that leads
    /// this turn.
    pub(super) fn adoption_candidate_in_memory(
        &self,
        session_key: &str,
        head: &str,
        canonical_current: &[Value],
    ) -> Option<AdoptedPrefix> {
        let keys: Vec<String> = self
            .head_index
            .lock()
            .ok()?
            .get(head)
            .cloned()
            .unwrap_or_default();
        let guard = self.trackers.lock().ok()?;
        let mut best: Option<AdoptedPrefix> = None;
        for key in keys.iter().filter(|k| k.as_str() != session_key) {
            let Some(tracker) = guard.peek(key) else {
                continue;
            };
            if tracker.last_activity.elapsed() > self.session_ttl {
                continue;
            }
            let found = std::iter::once((
                &tracker.last_original_messages,
                &tracker.last_forwarded_messages,
                tracker.last_forwarded_system_hash.as_str(),
            ))
            .chain(
                tracker
                    .alternates
                    .iter()
                    .map(|(_, o, f, s)| (o, f, s.as_str())),
            )
            .filter_map(|(o, f, s)| adoptable_len(o, f, canonical_current).map(|n| (n, o, f, s)))
            .max_by_key(|(n, ..)| *n);
            let Some((n, o, f, s)) = found else {
                tracing::debug!(
                    event = "prefix_adoption_candidate_rejected",
                    donor_session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(key),
                    source = "memory",
                    donor_prefix_msgs = tracker.last_original_messages.len(),
                    incoming_msgs = canonical_current.len(),
                    "a session sharing this turn's opening messages holds no prefix of it"
                );
                continue;
            };
            let candidate = AdoptedPrefix {
                originals: o[..n].to_vec(),
                forwarded: f[..n].to_vec(),
                forwarded_system_hash: s.to_string(),
                cached_token_count: tracker.cached_token_count,
                cached_message_count: tracker.cached_message_count,
                turn_number: tracker.turn_number,
                donor: AdoptionDonor::Session(key.clone()),
                age: tracker.last_activity.elapsed(),
            };
            if candidate.beats(best.as_ref()) {
                best = Some(candidate);
            }
        }
        best
    }

    /// Newest live lane (in memory) whose tracked messages this turn's history
    /// extends — the lineage hold pins should inherit along on a lane switch.
    ///
    /// Same bar as adoption ([`CROSS_SESSION_ADOPT_MIN_MESSAGES`], head hash,
    /// TTL, longest-wins/recent-tiebreak): inheritance fires exactly when
    /// adoption would find a donor, so a pin can never travel to an unrelated
    /// stream. Memory only: pin stores are keyed by lane key while persisted
    /// files are named by its digest, which is irreversible — and a
    /// restart-and-`cd` in the same turn is vanishingly rarer than either
    /// alone. The adoption gate still protects the turn when no pin travels.
    pub(crate) fn lineage_donor_lane(
        &self,
        session_key: &str,
        current_originals: &[Value],
    ) -> Option<String> {
        if current_originals.len() < CROSS_SESSION_ADOPT_MIN_MESSAGES {
            return None;
        }
        let head = adoption_head_hash(current_originals)?;
        let canonical_current = canonicalize_slice(current_originals);
        let keys: Vec<String> = self
            .head_index
            .lock()
            .ok()?
            .get(&head)
            .cloned()
            .unwrap_or_default();
        let guard = self.trackers.lock().ok()?;
        // Best (n, age, key) — mirrors [`AdoptedPrefix::beats`] without
        // cloning message bodies: this runs pre-preview on every long turn,
        // and the bodies are only needed if adoption itself fires later.
        let mut best: Option<(usize, Duration, String)> = None;
        for key in keys.iter().filter(|k| k.as_str() != session_key) {
            let Some(tracker) = guard.peek(key) else {
                continue;
            };
            if tracker.last_activity.elapsed() > self.session_ttl {
                continue;
            }
            let n = std::iter::once((
                &tracker.last_original_messages,
                &tracker.last_forwarded_messages,
            ))
            .chain(tracker.alternates.iter().map(|(_, o, f, _)| (o, f)))
            .filter_map(|(o, f)| adoptable_len(o, f, &canonical_current))
            .max()
            .unwrap_or(0);
            if n == 0 {
                continue;
            }
            let age = tracker.last_activity.elapsed();
            let wins = match &best {
                None => true,
                Some((best_n, best_age, _)) => n > *best_n || (n == *best_n && age < *best_age),
            };
            if wins {
                best = Some((n, age, key.clone()));
            }
        }
        best.map(|(_, _, key)| key)
    }

    /// The longest persisted prefix of another session that leads this turn.
    ///
    /// Every file's head hash is read once per modification and remembered,
    /// so after the first pass a scan costs a directory listing, a metadata
    /// call per file, and a parse of only the files whose head matches.
    pub(super) fn adoption_candidate_on_disk(
        &self,
        session_key: &str,
        head: &str,
        canonical_current: &[Value],
    ) -> Option<AdoptedPrefix> {
        let dir = self.persist_dir.as_deref()?;
        let own = persisted_path(dir, session_key);
        let entries = std::fs::read_dir(dir).ok()?;
        let mut best: Option<AdoptedPrefix> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == own || path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            let remembered = self
                .persisted_heads
                .lock()
                .ok()
                .and_then(|g| g.get(&path).cloned());
            let snapshot = match remembered {
                Some((seen_at, file_head)) if seen_at == modified => {
                    if file_head.as_deref() != Some(head) {
                        continue;
                    }
                    read_persisted_prefix_at(&path)
                }
                _ => {
                    let snapshot = read_persisted_prefix_at(&path);
                    let file_head = snapshot.as_ref().and_then(|s| {
                        s.head_hash
                            .clone()
                            .or_else(|| adoption_head_hash(&s.originals))
                    });
                    if let Ok(mut g) = self.persisted_heads.lock() {
                        g.insert(path.clone(), (modified, file_head.clone()));
                    }
                    if file_head.as_deref() != Some(head) {
                        continue;
                    }
                    snapshot
                }
            };
            let Some(snapshot) = snapshot else {
                continue;
            };
            let digest = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let Some(n) =
                adoptable_len(&snapshot.originals, &snapshot.forwarded, canonical_current)
            else {
                tracing::debug!(
                    event = "prefix_adoption_candidate_rejected",
                    donor_session_key_hash = %digest.chars().take(16).collect::<String>(),
                    source = "disk",
                    donor_prefix_msgs = snapshot.originals.len(),
                    incoming_msgs = canonical_current.len(),
                    "a persisted prefix sharing this turn's opening messages does not lead it"
                );
                continue;
            };
            let mut originals = snapshot.originals;
            let mut forwarded = snapshot.forwarded;
            originals.truncate(n);
            forwarded.truncate(n);
            let candidate = AdoptedPrefix {
                originals,
                forwarded,
                // Files written before the field existed carry `None` and
                // decline at the gate (empty never equals a real digest) —
                // one honest miss until the session turns again and rewrites
                // the file with the digest.
                forwarded_system_hash: snapshot.forwarded_system_hash.unwrap_or_default(),
                cached_token_count: snapshot.cached_token_count,
                cached_message_count: snapshot.cached_message_count,
                turn_number: snapshot.turn_number,
                donor: AdoptionDonor::PersistedDigest(digest),
                age: Duration::from_secs(unix_now().saturating_sub(snapshot.saved_at_unix)),
            };
            if candidate.beats(best.as_ref()) {
                best = Some(candidate);
            }
        }
        best
    }

    /// Seed `session_key` with the adopted slice: as its primary prefix when it
    /// holds nothing live, otherwise as the newest alternate, which
    /// [`Self::previous_turn_for`] and `update_from_response` both consult.
    pub(super) fn install_adopted(&self, session_key: &str, adopted: AdoptedPrefix) {
        let head = adoption_head_hash(&adopted.originals);
        let Ok(mut guard) = self.trackers.lock() else {
            return;
        };
        let live = guard.peek(session_key).is_some_and(|t| {
            t.last_activity.elapsed() <= self.session_ttl && !t.last_forwarded_messages.is_empty()
        });
        if live {
            let tracker = guard.get_mut(session_key).expect("peeked above");
            let id = tracker.next_chain_id;
            tracker.next_chain_id += 1;
            tracker
                .alternates
                .retain(|(_, o, _, _)| o != &adopted.originals);
            tracker.alternates.insert(
                0,
                (
                    id,
                    adopted.originals,
                    adopted.forwarded,
                    adopted.forwarded_system_hash,
                ),
            );
            tracker.alternates.truncate(MAX_ALTERNATE_PREFIXES);
            tracker.last_activity = Instant::now();
        } else {
            let cached_message_count = adopted.cached_message_count.min(adopted.forwarded.len());
            let evicted = guard.push(
                session_key.to_string(),
                PrefixReplayTracker {
                    cached_token_count: adopted.cached_token_count,
                    cached_message_count,
                    turn_number: adopted.turn_number.max(1),
                    last_activity: Instant::now(),
                    last_original_messages: adopted.originals,
                    last_forwarded_messages: adopted.forwarded,
                    last_forwarded_system_hash: adopted.forwarded_system_hash,
                    alternates: Vec::new(),
                    primary_chain_id: 1,
                    next_chain_id: 2,
                },
            );
            self.unindex_evicted(session_key, evicted);
        }
        drop(guard);
        self.index_head(session_key, head);
    }

    #[cfg(test)]
    pub(super) fn active_sessions(&self) -> usize {
        self.trackers.lock().map(|g| g.len()).unwrap_or(0)
    }
}
