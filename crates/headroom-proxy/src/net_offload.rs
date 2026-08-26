//! Startup check for the NIC offload settings that corrupt TLS records.
//!
//! `docs/tls-record-corruption-wsl2.md` has the full account. In short: on
//! WSL2's virtual NIC, `generic-receive-offload` merges inbound packets before
//! the kernel sees them and `rx-checksumming` lets the NIC assert a checksum
//! the kernel then skips verifying, so a corrupted byte travels all the way up
//! to TLS. TLS cannot repair a record, so it kills the connection — and with
//! HTTP/2 that takes every stream multiplexed on it at once.
//!
//! The damage lands on agent work in particular. A dropped `tool_use` block is
//! discarded rather than half-delivered, so a lost `Agent` call reads as a
//! subagent that ran and reported nothing. Whole turns are paid for and thrown
//! away.
//!
//! None of that is repairable once a session is under way, which is why this
//! runs at startup: the settings are wrong for the whole session or right for
//! the whole session, and the operator can only act on it beforehand.
//!
//! # Why it reads the routing table
//!
//! The fix used to be written down against `eth0`. Under
//! `networkingMode=mirrored` that interface is down and the Windows adapters
//! are mirrored in beside it, so the live NIC is whichever one holds the
//! default route — and its name moves as the machine changes network. Checking
//! a fixed name is how this went unnoticed for two days on 2026-08-26. So the
//! interface is looked up, never assumed.
//!
//! # Why it only warns
//!
//! Turning offload off needs root and bounces the NIC queues, which is not
//! something to do to a machine underneath a running session. The commands go
//! in the log for the operator to run between turns.

use std::process::Command;

/// Offload features that let a corrupted byte reach TLS.
///
/// `gro` and `lro` merge inbound packets before the kernel sees them; `rx`
/// checksumming is what lets the damage through unverified. The transmit-side
/// pair (`tso`, `gso`) is turned off alongside them in the fix because it costs
/// nothing, but it is not part of the receive path and is not checked here — a
/// warning has to name a cause, not everything the remedy touches.
const RISKY_FEATURES: &[(&str, &str)] = &[
    ("generic-receive-offload", "gro"),
    ("large-receive-offload", "lro"),
    ("rx-checksumming", "rx"),
];

/// Whether this kernel is WSL.
///
/// Offload is ordinary and wanted on real hardware, so the warning would be
/// noise anywhere else. Microsoft's kernels carry the marker in the release
/// string.
fn is_wsl(osrelease: &str) -> bool {
    let lower = osrelease.to_ascii_lowercase();
    lower.contains("microsoft") || lower.contains("wsl")
}

/// The interface holding the default route, from `/proc/net/route`.
///
/// A default route is the one with an all-zero destination. Several can exist
/// at once — a VPN beside a hotspot is the usual case — so the lowest metric
/// wins, which is the one the kernel will actually use.
fn default_route_interface(proc_net_route: &str) -> Option<String> {
    proc_net_route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let iface = fields.next()?;
            let destination = fields.next()?;
            if destination != "00000000" {
                return None;
            }
            // Gateway, Flags, RefCnt, Use, then Metric.
            let metric = fields.nth(4).and_then(|m| m.parse::<u32>().ok())?;
            Some((metric, iface.to_string()))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, iface)| iface)
}

/// The risky features `ethtool -k` reports as on.
///
/// Lines read `generic-receive-offload: on [fixed]`. A feature the driver
/// pins off is reported `off [fixed]` and cannot be the cause, so matching on
/// the word after the colon is enough.
fn risky_offloads_on(ethtool_output: &str) -> Vec<&'static str> {
    RISKY_FEATURES
        .iter()
        .filter(|(feature, _)| {
            ethtool_output.lines().any(|line| {
                let mut parts = line.trim().splitn(2, ": ");
                parts.next() == Some(*feature)
                    && parts
                        .next()
                        .is_some_and(|value| value.split_whitespace().next() == Some("on"))
            })
        })
        .map(|(_, flag)| *flag)
        .collect()
}

/// Every interface the kernel knows about, loopback aside.
///
/// Checking only the one holding the default route is not enough, and the
/// reason is the whole shape of this fault: the hotspot, the wifi and the VPN
/// are all mirrored in at once, and the route moves between them without
/// anything restarting. An interface that is idle at startup can be carrying
/// every token by the middle of the session. So all of them are checked, and
/// the one on the route is only singled out for urgency.
fn interfaces() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| !is_loopback(name))
        .collect();
    names.sort();
    names
}

/// Whether an interface is loopback, by its ARP type rather than its name.
///
/// `lo` is not the only one: mirrored networking adds `loopback0` beside it,
/// and warning about an interface whose traffic never reaches a wire is pure
/// noise in the one message that has to be worth reading.
fn is_loopback(interface: &str) -> bool {
    // WSL's mirrored loopback reports ARPHRD_ETHER and carries no `IFF_LOOPBACK`
    // flag — it is indistinguishable from a real adapter except by name, so the
    // name is what excludes it. Its traffic never reaches a wire and it cannot
    // hold a route off the machine, so there is nothing there to corrupt.
    if interface == "loopback0" {
        return true;
    }
    // ARPHRD_LOOPBACK, for the real one. Unreadable means "not knowably
    // loopback", which keeps a real interface in the check rather than
    // dropping it on a failed read.
    std::fs::read_to_string(format!("/sys/class/net/{interface}/type"))
        .map(|kind| kind.trim() == "772")
        .unwrap_or(false)
}

/// The risky offloads on one interface, or `None` when it cannot be read.
fn offloads_on(interface: &str) -> Option<Vec<&'static str>> {
    let output = Command::new("ethtool").arg("-k").arg(interface).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(risky_offloads_on(&String::from_utf8_lossy(&output.stdout)))
}

/// One `ethtool` line that turns off everything named, in the form the fix is
/// written down in.
fn remedy(interface: &str, risky: &[&str]) -> String {
    let flags = risky
        .iter()
        .map(|flag| format!("{flag} off"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("sudo ethtool -K {interface} {flags}")
}

/// Read every interface's offload state and say, loudly, which of them are in
/// the state that corrupts TLS records.
///
/// Silent on anything that is not WSL, and silent about an interface whose
/// state cannot be read — `ethtool` is not installed everywhere, and a check
/// that cannot run is not evidence of a fault.
pub fn warn_if_offload_corrupts_tls() {
    let Ok(osrelease) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        return;
    };
    if !is_wsl(&osrelease) {
        return;
    }
    let on_route = std::fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|table| default_route_interface(&table));

    let mut risky: Vec<(String, Vec<&'static str>)> = Vec::new();
    let mut checked = 0usize;
    for interface in interfaces() {
        let Some(offloads) = offloads_on(&interface) else {
            continue;
        };
        checked += 1;
        if !offloads.is_empty() {
            risky.push((interface, offloads));
        }
    }
    if checked == 0 {
        tracing::debug!(
            event = "nic_offload_check_skipped",
            reason = "ethtool_unavailable",
            "cannot check NIC offload: ethtool did not run for any interface"
        );
        return;
    }
    if risky.is_empty() {
        tracing::info!(
            event = "nic_offload_ok",
            interfaces_checked = checked,
            default_route = %on_route.as_deref().unwrap_or("none"),
            "NIC offload is off on every interface"
        );
        return;
    }
    // The interface on the route is the one already costing tokens; the rest
    // are what the next network change will cost. Both go in the same warning,
    // because fixing only the first is what left the VPN armed on 2026-08-26.
    let live = on_route
        .as_deref()
        .filter(|name| risky.iter().any(|(interface, _)| interface == name));
    let commands = risky
        .iter()
        .map(|(interface, offloads)| remedy(interface, offloads))
        .collect::<Vec<_>>()
        .join("; ");
    tracing::warn!(
        event = "nic_offload_risky",
        interfaces = %risky.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(","),
        on_default_route = %live.unwrap_or("no"),
        remedy = %commands,
        "receive offload is on for {}, which corrupts TLS records under WSL2 and \
         kills whole turns mid-stream — a dropped tool call reads as a subagent \
         that reported nothing.{} Run these BEFORE starting a session, between \
         turns rather than mid-stream: {}. To make it survive reboots and \
         network changes see docs/tls-record-corruption-wsl2.md",
        risky.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(", "),
        match live {
            Some(name) =>
                format!(" {name} is carrying upstream traffic right now, so this is live."),
            None => " None is on the default route yet, so this is what the next \
                     network change will cost."
                .to_string(),
        },
        commands,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_is_recognised_and_real_hardware_is_not() {
        assert!(is_wsl("6.6.87.2-microsoft-standard-WSL2"));
        assert!(!is_wsl("6.8.0-45-generic"));
    }

    /// The lowest metric wins: a VPN beside a hotspot is the ordinary case, and
    /// warning about the wrong interface is worse than not warning.
    #[test]
    fn the_default_route_picks_the_live_interface() {
        let table = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
eth4\t00000000\t0100050A\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth3\t00000000\t0114140A\t0003\t0\t0\t45\t00000000\t0\t0\t0
eth3\t0014140A\t00000000\t0001\t0\t0\t0\t000000F0\t0\t0\t0
";
        assert_eq!(default_route_interface(table).as_deref(), Some("eth3"));
    }

    #[test]
    fn no_default_route_is_not_an_interface() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
             eth0\t0014140A\t00000000\t0001\t0\t0\t0\t000000F0\n";
        assert_eq!(default_route_interface(table), None);
    }

    /// The state that was live on 2026-08-26 when the corruption came back.
    #[test]
    fn offload_left_on_is_reported() {
        let features = "\
Features for eth3:
rx-checksumming: on
tx-checksumming: on
tcp-segmentation-offload: on
generic-segmentation-offload: on
generic-receive-offload: on
large-receive-offload: on
";
        assert_eq!(risky_offloads_on(features), vec!["gro", "lro", "rx"]);
    }

    #[test]
    fn offload_turned_off_is_clean() {
        let features = "\
Features for eth3:
rx-checksumming: off
generic-receive-offload: off
large-receive-offload: off [fixed]
tcp-segmentation-offload: on
";
        assert!(risky_offloads_on(features).is_empty());
    }

    /// A driver that pins a feature off cannot be the cause, and `off [fixed]`
    /// must not be read as `on` by a looser match.
    #[test]
    fn fixed_annotations_do_not_confuse_the_match() {
        assert_eq!(
            risky_offloads_on("generic-receive-offload: on [fixed]\n"),
            vec!["gro"]
        );
        assert!(risky_offloads_on("generic-receive-offload: off [fixed]\n").is_empty());
    }

    /// A feature name that merely contains another must not match it.
    #[test]
    fn a_longer_feature_name_is_not_a_match() {
        assert!(risky_offloads_on("tx-generic-receive-offload: on\n").is_empty());
    }

    /// The remedy has to be runnable as printed — this is the whole value of
    /// the warning, and a mangled flag list is worse than none.
    #[test]
    fn the_remedy_is_a_command_that_can_be_pasted() {
        assert_eq!(
            remedy("eth4", &["gro", "lro", "rx"]),
            "sudo ethtool -K eth4 gro off lro off rx off"
        );
        assert_eq!(remedy("eth3", &["gro"]), "sudo ethtool -K eth3 gro off");
    }

    /// Loopback has no offload worth checking and would only add noise. Under
    /// mirrored networking there is more than one of them, so the ARP type
    /// decides it rather than the name.
    #[test]
    fn loopback_is_not_checked() {
        let checked = interfaces();
        assert!(!checked.contains(&"lo".to_string()));
        assert!(
            !checked.iter().any(|name| is_loopback(name)),
            "a loopback interface survived the filter: {checked:?}"
        );
    }
}
