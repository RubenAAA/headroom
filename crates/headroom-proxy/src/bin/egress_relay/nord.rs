//! The Nord SOCKS server list: fetched from Nord, cached, ranked by health.
//!
//! Nord accepts this account's credentials on only some of its SOCKS servers,
//! and the accepted set changes within minutes (measured 2026-09-29: 11 of 16
//! listed servers accepted, then 3 of 25 five minutes later, with us28-us33 in
//! both sets). A fixed list therefore strands lanes on servers that reject
//! them. The book keeps the current listing, remembers which hosts last
//! answered a probe, and orders rotation candidates by that.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// First-run list, used until Nord's own listing has been fetched.
pub const SEED: &[&str] = &[
    "socks-us34.nordvpn.com",
    "socks-us35.nordvpn.com",
    "socks-us36.nordvpn.com",
    "socks-us37.nordvpn.com",
    "socks-us45.nordvpn.com",
    "socks-us46.nordvpn.com",
    "socks-us47.nordvpn.com",
    "socks-us48.nordvpn.com",
    "socks-us49.nordvpn.com",
    "socks-us50.nordvpn.com",
    "socks-us28.nordvpn.com",
    "socks-us29.nordvpn.com",
    "socks-us30.nordvpn.com",
    "socks-us31.nordvpn.com",
    "socks-us32.nordvpn.com",
    "socks-us33.nordvpn.com",
];

pub const LISTING_URL: &str =
    "https://api.nordvpn.com/v1/servers?limit=5000&filters[servers_technologies][identifier]=socks";

/// A listing shorter than this is treated as an API fault, not a shrunken fleet.
const MIN_LISTED: usize = 8;
/// A probe success counts as current for this long.
const OK_FRESH: Duration = Duration::from_secs(2 * 3600);
/// A probe failure keeps a host at the back of the queue for this long.
const FAILED_RECENT: Duration = Duration::from_secs(600);

/// Credentials are sent to whatever this accepts, so it is strict: the exact
/// shape of Nord's SOCKS hostnames and nothing else.
pub fn is_nord_socks_host(host: &str) -> bool {
    let Some(name) = host
        .strip_prefix("socks-")
        .and_then(|rest| rest.strip_suffix(".nordvpn.com"))
    else {
        return false;
    };
    let digits = name.trim_start_matches(|c: char| c.is_ascii_lowercase());
    let letters = name.len() - digits.len();
    (1..=3).contains(&letters)
        && (1..=4).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `(hostname, load)` of every online SOCKS server in Nord's `/v1/servers`
/// answer. Anything else in it, including hosts that fail `is_nord_socks_host`,
/// is dropped.
pub fn parse_listing(body: &Value) -> Vec<(String, u64)> {
    let mut seen = HashSet::new();
    body.as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|server| server.get("status").and_then(Value::as_str) == Some("online"))
        .filter_map(|server| {
            let host = server.get("hostname")?.as_str()?;
            let load = server.get("load").and_then(Value::as_u64).unwrap_or(100);
            (is_nord_socks_host(host) && seen.insert(host.to_string()))
                .then(|| (host.to_string(), load))
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Health {
    ok: Option<Instant>,
    failed: Option<Instant>,
    checked: Option<Instant>,
    /// Smoothed TCP round trip to the server, from this machine.
    rtt: Option<Duration>,
}

pub struct ServerBook {
    hosts: Vec<String>,
    health: HashMap<String, Health>,
}

impl ServerBook {
    pub fn seeded() -> Self {
        Self {
            hosts: SEED.iter().map(|h| h.to_string()).collect(),
            health: HashMap::new(),
        }
    }

    /// The book saved by `to_cache`. `None` for anything that is not a list of
    /// at least `MIN_LISTED` valid hostnames, so a damaged file means "use the seed".
    pub fn from_cache(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let hosts: Vec<String> = value
            .get("hosts")?
            .as_array()?
            .iter()
            .filter_map(|h| h.as_str().filter(|h| is_nord_socks_host(h)))
            .map(str::to_string)
            .collect();
        (hosts.len() >= MIN_LISTED).then(|| Self {
            hosts,
            health: HashMap::new(),
        })
    }

    pub fn to_cache(&self) -> String {
        json!({"hosts": self.hosts}).to_string()
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// Fold a fresh listing in. Hosts still listed keep their place, new ones go
    /// last by load, hosts no longer listed are dropped unless a lane sits on
    /// them (`keep`). Returns whether anything changed; a listing too short to
    /// trust changes nothing.
    pub fn apply_listing(&mut self, listing: &[(String, u64)], keep: &HashSet<String>) -> bool {
        if listing.len() < MIN_LISTED {
            return false;
        }
        let listed: HashSet<&str> = listing.iter().map(|(h, _)| h.as_str()).collect();
        let mut next: Vec<String> = self
            .hosts
            .iter()
            .filter(|h| listed.contains(h.as_str()) || keep.contains(*h))
            .cloned()
            .collect();
        let known: HashSet<String> = next.iter().cloned().collect();
        let mut added: Vec<&(String, u64)> =
            listing.iter().filter(|(h, _)| !known.contains(h)).collect();
        added.sort_by_key(|(host, load)| (*load, host.clone()));
        next.extend(added.into_iter().map(|(h, _)| h.clone()));
        self.health.retain(|h, _| next.contains(h));
        let changed = next != self.hosts;
        self.hosts = next;
        changed
    }

    /// Record a measured round trip. Smoothed, so one slow sample moves the
    /// ranking by half, not all the way.
    pub fn note_rtt(&mut self, host: &str, rtt: Duration) {
        let entry = self.health.entry(host.to_string()).or_default();
        entry.rtt = Some(entry.rtt.map_or(rtt, |old| (old + rtt) / 2));
    }

    /// Record a probe result.
    pub fn mark(&mut self, host: &str, ok: bool) {
        let entry = self.health.entry(host.to_string()).or_default();
        let now = Instant::now();
        entry.checked = Some(now);
        if ok {
            entry.ok = Some(now);
        } else {
            entry.failed = Some(now);
        }
    }

    /// 0: answered a probe recently and nothing since. 1: unknown. 2: failed
    /// recently. Rank 2 stays in the queue: the fault may have cleared, and the
    /// rotation probes each candidate before using it anyway.
    fn rank(&self, host: &str) -> u8 {
        let Some(h) = self.health.get(host) else {
            return 1;
        };
        let newest_failure = h.failed.filter(|f| h.ok.is_none_or(|ok| *f > ok));
        match (h.ok, newest_failure) {
            (Some(ok), None) if ok.elapsed() < OK_FRESH => 0,
            (_, Some(f)) if f.elapsed() < FAILED_RECENT => 2,
            _ => 1,
        }
    }

    /// Rotation candidates for a lane on `after`: minus hosts other lanes hold,
    /// best health rank first, then lowest round trip (unmeasured last), with
    /// ring order from `after` breaking ties.
    pub fn candidates(&self, after: &str, exclude: &HashSet<String>) -> Vec<String> {
        let start = self
            .hosts
            .iter()
            .position(|h| h == after)
            .map_or(0, |i| i + 1);
        let mut ring: Vec<String> = (0..self.hosts.len())
            .map(|offset| self.hosts[(start + offset) % self.hosts.len()].clone())
            .filter(|h| h != after && !exclude.contains(h))
            .collect();
        ring.sort_by_key(|h| {
            (
                self.rank(h),
                self.health
                    .get(h)
                    .and_then(|s| s.rtt)
                    .unwrap_or(Duration::MAX),
            )
        });
        ring
    }

    /// Up to `n` hosts to probe in a sweep: never-checked first, then the
    /// longest-unchecked. Hosts a lane holds are left to the lane check.
    pub fn sweep_targets(&self, exclude: &HashSet<String>, n: usize) -> Vec<String> {
        let mut hosts: Vec<&String> = self
            .hosts
            .iter()
            .filter(|h| !exclude.contains(*h))
            .collect();
        hosts.sort_by_key(|h| self.health.get(*h).and_then(|s| s.checked));
        hosts.into_iter().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(hosts: &[&str]) -> HashSet<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    fn listing(range: std::ops::Range<u32>) -> Vec<(String, u64)> {
        range
            .map(|n| (format!("socks-us{n}.nordvpn.com"), u64::from(n)))
            .collect()
    }

    #[test]
    fn only_nord_socks_hostnames_may_receive_credentials() {
        for good in [
            "socks-us34.nordvpn.com",
            "socks-nl1.nordvpn.com",
            "socks-se15.nordvpn.com",
        ] {
            assert!(is_nord_socks_host(good), "{good}");
        }
        for bad in [
            "socks-us34.nordvpn.com.evil.net",
            "evil.net",
            "socks-us34.nordvpn.com:1080",
            "socks-.nordvpn.com",
            "socks-us.nordvpn.com",
            "socks-us3x.nordvpn.com",
            "socks-us34.nordvpn.co",
            "us34.nordvpn.com",
            "",
        ] {
            assert!(!is_nord_socks_host(bad), "{bad}");
        }
    }

    #[test]
    fn a_listing_drops_offline_duplicate_and_foreign_hosts() {
        let body = json!([
            {"hostname": "socks-us1.nordvpn.com", "status": "online", "load": 5},
            {"hostname": "socks-us1.nordvpn.com", "status": "online", "load": 9},
            {"hostname": "socks-us2.nordvpn.com", "status": "offline", "load": 1},
            {"hostname": "evil.example.com", "status": "online", "load": 1},
            {"status": "online"},
            "garbage",
        ]);
        assert_eq!(
            parse_listing(&body),
            vec![("socks-us1.nordvpn.com".to_string(), 5)]
        );
        assert!(parse_listing(&json!({"error": "rate limited"})).is_empty());
    }

    #[test]
    fn a_short_listing_is_an_api_fault_and_changes_nothing() {
        let mut book = ServerBook::seeded();
        assert!(!book.apply_listing(&listing(1..4), &HashSet::new()));
        assert_eq!(book.len(), SEED.len());
    }

    #[test]
    fn a_listing_adds_new_hosts_by_load_and_keeps_hosts_a_lane_sits_on() {
        let mut book = ServerBook::seeded();
        let mut fresh = listing(100..110);
        fresh.push(("socks-us34.nordvpn.com".to_string(), 3));
        // Everything else in the seed is gone from the listing.
        let keep = set(&["socks-us45.nordvpn.com"]);
        assert!(book.apply_listing(&fresh, &keep));
        let hosts = book.candidates("", &HashSet::new());
        assert!(
            hosts.contains(&"socks-us45.nordvpn.com".to_string()),
            "lane host kept"
        );
        assert!(
            !hosts.contains(&"socks-us46.nordvpn.com".to_string()),
            "unlisted host dropped"
        );
        assert_eq!(book.len(), 1 + 1 + 10);
        // The new hosts follow the surviving ones, lowest load first.
        assert_eq!(hosts.last().unwrap(), "socks-us109.nordvpn.com");
    }

    #[test]
    fn candidates_put_recent_successes_first_and_recent_failures_last() {
        let mut book = ServerBook::seeded();
        book.mark("socks-us33.nordvpn.com", true);
        book.mark("socks-us35.nordvpn.com", false);
        let got = book.candidates("socks-us34.nordvpn.com", &set(&["socks-us36.nordvpn.com"]));
        assert_eq!(got.first().unwrap(), "socks-us33.nordvpn.com");
        assert_eq!(got.last().unwrap(), "socks-us35.nordvpn.com");
        assert!(
            !got.contains(&"socks-us34.nordvpn.com".to_string()),
            "own host excluded"
        );
        assert!(
            !got.contains(&"socks-us36.nordvpn.com".to_string()),
            "held host excluded"
        );
    }

    #[test]
    fn within_a_health_rank_the_lowest_round_trip_goes_first() {
        let mut book = ServerBook::seeded();
        for (host, ms) in [("us33", 180), ("us35", 40), ("us36", 90)] {
            let host = format!("socks-{host}.nordvpn.com");
            book.mark(&host, true);
            book.note_rtt(&host, Duration::from_millis(ms));
        }
        let got = book.candidates("socks-us34.nordvpn.com", &HashSet::new());
        assert_eq!(
            &got[..3],
            [
                "socks-us35.nordvpn.com",
                "socks-us36.nordvpn.com",
                "socks-us33.nordvpn.com"
            ]
        );
        // A fast server that failed its probe still ranks behind a slow one that passed.
        book.mark("socks-us35.nordvpn.com", false);
        let got = book.candidates("socks-us34.nordvpn.com", &HashSet::new());
        assert_eq!(got[0], "socks-us36.nordvpn.com");
    }

    #[test]
    fn one_slow_sample_moves_the_smoothed_round_trip_by_half() {
        let mut book = ServerBook::seeded();
        book.note_rtt("socks-us33.nordvpn.com", Duration::from_millis(50));
        book.note_rtt("socks-us33.nordvpn.com", Duration::from_millis(250));
        assert_eq!(
            book.health["socks-us33.nordvpn.com"].rtt,
            Some(Duration::from_millis(150))
        );
    }

    #[test]
    fn a_success_after_a_failure_clears_the_failure() {
        let mut book = ServerBook::seeded();
        book.mark("socks-us33.nordvpn.com", false);
        book.mark("socks-us33.nordvpn.com", true);
        assert_eq!(book.rank("socks-us33.nordvpn.com"), 0);
    }

    #[test]
    fn a_damaged_cache_falls_back_and_a_good_one_round_trips() {
        assert!(ServerBook::from_cache("not json").is_none());
        assert!(ServerBook::from_cache(r#"{"hosts":["evil.net"]}"#).is_none());
        assert!(ServerBook::from_cache(r#"{"hosts":["socks-us1.nordvpn.com"]}"#).is_none());
        let restored = ServerBook::from_cache(&ServerBook::seeded().to_cache()).unwrap();
        assert_eq!(restored.len(), SEED.len());
    }

    #[test]
    fn sweeps_probe_unchecked_hosts_before_stale_ones() {
        let mut book = ServerBook::seeded();
        for host in SEED.iter().skip(2) {
            book.mark(host, true);
        }
        let got = book.sweep_targets(&HashSet::new(), 2);
        assert_eq!(got, vec![SEED[0].to_string(), SEED[1].to_string()]);
    }
}
