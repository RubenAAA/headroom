//! Agent token-savings profiles for the CLI (Rust port of the CLI slice of
//! `headroom/agent_savings.py` + `headroom/cli/agent_savings.py`). Covers the
//! profile table, `proxy_env()` rendering, and the --check-perf gate helpers;
//! the proxy-side config application lives in
//! `headroom_core::transforms::content_router::SavingsProfile`.

use std::path::{Path, PathBuf};

use headroom_core::perf_analyzer::PerfRecord;
use serde_json::Value;

type Error = Box<dyn std::error::Error>;

/// Reusable policy for high-savings agent compression.
#[derive(Debug)]
pub struct AgentSavingsProfile {
    pub name: &'static str,
    pub target_savings: f64,
    /// None = don't pin a keep-ratio; let Kompress decide adaptively.
    pub target_ratio: Option<f64>,
    pub compress_user_messages: bool,
    pub compress_system_messages: bool,
    pub protect_recent: u32,
    pub protect_analysis_context: bool,
    pub min_tokens_to_compress: u32,
    pub max_items_after_crush: u32,
    pub smart_crusher_with_compaction: bool,
    pub force_kompress: bool,
    /// Forward `HEADROOM_PROTECT_READS`: keep file reads (`cat`, `sed -n`,
    /// `head`) byte-exact so the agent patches from what the file holds.
    pub protect_reads: bool,
    pub proxy_mode: &'static str,
    pub accuracy_guard: &'static str,
}

impl AgentSavingsProfile {
    /// Env vars for Headroom proxy/wrapper entry points, in the same order
    /// Python's `proxy_env()` dict yields them (shell output preserves it).
    pub fn proxy_env(&self) -> Vec<(&'static str, String)> {
        let flag = |b: bool| if b { "1" } else { "0" }.to_string();
        let mut env = vec![
            ("HEADROOM_MODE", self.proxy_mode.to_string()),
            ("HEADROOM_SAVINGS_PROFILE", self.name.to_string()),
            (
                "HEADROOM_SAVINGS_TARGET",
                format!("{:.2}", self.target_savings),
            ),
            (
                "HEADROOM_COMPRESS_USER_MESSAGES",
                flag(self.compress_user_messages),
            ),
            (
                "HEADROOM_COMPRESS_SYSTEM_MESSAGES",
                flag(self.compress_system_messages),
            ),
            ("HEADROOM_PROTECT_RECENT", self.protect_recent.to_string()),
            (
                "HEADROOM_PROTECT_ANALYSIS_CONTEXT",
                flag(self.protect_analysis_context),
            ),
            (
                "HEADROOM_MIN_TOKENS",
                self.min_tokens_to_compress.to_string(),
            ),
            ("HEADROOM_MAX_ITEMS", self.max_items_after_crush.to_string()),
            (
                "HEADROOM_SMART_CRUSHER_COMPACTION",
                flag(self.smart_crusher_with_compaction),
            ),
            ("HEADROOM_FORCE_KOMPRESS", flag(self.force_kompress)),
            ("HEADROOM_ACCURACY_GUARD", self.accuracy_guard.to_string()),
            ("HEADROOM_PROTECT_READS", flag(self.protect_reads)),
        ];
        if let Some(ratio) = self.target_ratio {
            env.push(("HEADROOM_TARGET_RATIO", format!("{ratio:.2}")));
        }
        env
    }
}

/// Sorted by name so the unknown-profile error lists them like Python does.
const PROFILES: &[AgentSavingsProfile] = &[
    AgentSavingsProfile {
        name: "agent-90",
        target_savings: 0.90,
        target_ratio: Some(0.10),
        compress_user_messages: true,
        compress_system_messages: true,
        protect_recent: 2,
        protect_analysis_context: true,
        min_tokens_to_compress: 120,
        max_items_after_crush: 8,
        smart_crusher_with_compaction: false,
        force_kompress: true,
        protect_reads: false,
        proxy_mode: "token",
        accuracy_guard: "strict",
    },
    AgentSavingsProfile {
        name: "balanced",
        target_savings: 0.70,
        target_ratio: Some(0.30),
        compress_user_messages: false,
        compress_system_messages: false,
        protect_recent: 4,
        protect_analysis_context: true,
        min_tokens_to_compress: 250,
        max_items_after_crush: 15,
        smart_crusher_with_compaction: true,
        force_kompress: false,
        protect_reads: false,
        proxy_mode: "token",
        accuracy_guard: "strict",
    },
    // Workload personas: target_savings is nominal (display only) — savings
    // emerge from lossless + relevance, and Kompress decides its own keep.
    AgentSavingsProfile {
        name: "coding",
        target_savings: 0.50,
        target_ratio: None,
        compress_user_messages: false,
        compress_system_messages: false,
        protect_recent: 2,
        protect_analysis_context: true,
        min_tokens_to_compress: 25,
        max_items_after_crush: 15,
        smart_crusher_with_compaction: true,
        force_kompress: false,
        // The one profile that turns it on: a coding session's working set is
        // the files it is about to patch.
        protect_reads: true,
        proxy_mode: "token",
        accuracy_guard: "strict",
    },
    AgentSavingsProfile {
        name: "general",
        target_savings: 0.60,
        target_ratio: None,
        compress_user_messages: false,
        compress_system_messages: false,
        protect_recent: 0,
        protect_analysis_context: true,
        min_tokens_to_compress: 25,
        max_items_after_crush: 15,
        smart_crusher_with_compaction: true,
        force_kompress: false,
        protect_reads: false,
        proxy_mode: "token",
        accuracy_guard: "strict",
    },
];

/// Return a named agent savings profile.
pub fn get_agent_savings_profile(name: &str) -> Result<&'static AgentSavingsProfile, String> {
    let key = name.trim().to_lowercase();
    let key = if key.is_empty() { "agent-90" } else { &key };
    PROFILES.iter().find(|p| p.name == key).ok_or_else(|| {
        let valid: Vec<&str> = PROFILES.iter().map(|p| p.name).collect();
        format!(
            "unknown savings profile '{name}'; expected one of: {}",
            valid.join(", ")
        )
    })
}

/// Read `totals.accuracy_rate` (or top-level `accuracy_preservation_rate`)
/// from a Headroom eval JSON report.
pub fn read_accuracy_rate(path: &Path) -> Result<f64, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let payload: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let as_float = |v: &Value| -> Option<f64> {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    if let Some(rate) = payload
        .get("totals")
        .and_then(|t| t.get("accuracy_rate"))
        .and_then(as_float)
    {
        return Ok(rate);
    }
    if let Some(rate) = payload.get("accuracy_preservation_rate").and_then(as_float) {
        return Ok(rate);
    }
    Err(format!(
        "{} does not contain totals.accuracy_rate or accuracy_preservation_rate",
        path.display()
    )
    .into())
}

/// Parse `--require-agents` into normalized client names.
pub fn split_required_agents(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Verify each required client's aggregate savings meets the target percent.
pub fn check_required_agents(
    records: &[PerfRecord],
    required_agents: &[String],
    target_percent: f64,
) -> Result<Vec<String>, Error> {
    let client_of = |r: &PerfRecord| r.client.trim().to_lowercase();
    let missing: Vec<&str> = required_agents
        .iter()
        .filter(|agent| !records.iter().any(|r| &client_of(r) == *agent))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing required agent traffic: {}", missing.join(", ")).into());
    }

    let mut messages = Vec::new();
    for agent in required_agents {
        let (before, saved) = records
            .iter()
            .filter(|r| &client_of(r) == agent)
            .fold((0i64, 0i64), |(b, s), r| {
                (b + r.tokens_before, s + r.tokens_saved)
            });
        let measured = if before > 0 {
            saved as f64 / before as f64 * 100.0
        } else {
            0.0
        };
        if measured < target_percent {
            return Err(format!(
                "{agent}: {measured:.1}% savings below {target_percent:.1}% target"
            )
            .into());
        }
        messages.push(format!(
            "{agent}: {measured:.1}% savings meets {target_percent:.1}% target"
        ));
    }
    Ok(messages)
}

/// Write the deterministic three-agent PERF/eval fixture; returns the eval
/// report path.
pub fn write_smoke_fixture(workspace: &Path) -> Result<PathBuf, Error> {
    let logs_dir = workspace.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let perf_lines = [
        perf_line(
            "2026-06-10 10:00:00,000",
            "hr_smoke_claude",
            "claude-sonnet",
            "claude",
            1000,
            80,
        ),
        perf_line(
            "2026-06-10 10:01:00,000",
            "hr_smoke_codex",
            "gpt-5",
            "codex",
            1000,
            90,
        ),
        perf_line(
            "2026-06-10 10:02:00,000",
            "hr_smoke_cursor",
            "gpt-5",
            "cursor",
            1000,
            70,
        ),
    ];
    std::fs::write(logs_dir.join("proxy.log"), perf_lines.join("\n") + "\n")?;
    let eval_path = workspace.join("agent-90-eval.json");
    let totals = serde_json::json!({
        "totals": {
            "cases": 3,
            "passed": 3,
            "accuracy_rate": 1.0,
            "tokens_original": 3000,
            "tokens_compressed": 240,
        }
    });
    std::fs::write(&eval_path, serde_json::to_string_pretty(&totals)? + "\n")?;
    Ok(eval_path)
}

fn perf_line(
    timestamp: &str,
    request_id: &str,
    model: &str,
    client: &str,
    before: i64,
    after: i64,
) -> String {
    let saved = before - after;
    format!(
        "{timestamp} - headroom.proxy - INFO - [{request_id}] PERF \
         model={model} msgs=3 tok_before={before} tok_after={after} \
         tok_saved={saved} cache_read=0 cache_write=0 cache_hit_pct=0 \
         opt_ms=1 transforms=agent90_smoke client={client}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_90_exports_cross_agent_proxy_env() {
        let profile = get_agent_savings_profile("agent-90").unwrap();
        let env = profile.proxy_env();
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(get("HEADROOM_MODE"), "token");
        assert_eq!(get("HEADROOM_SAVINGS_PROFILE"), "agent-90");
        assert_eq!(get("HEADROOM_SAVINGS_TARGET"), "0.90");
        assert_eq!(get("HEADROOM_TARGET_RATIO"), "0.10");
        assert_eq!(get("HEADROOM_COMPRESS_USER_MESSAGES"), "1");
        assert_eq!(get("HEADROOM_COMPRESS_SYSTEM_MESSAGES"), "1");
        assert_eq!(get("HEADROOM_PROTECT_RECENT"), "2");
        assert_eq!(get("HEADROOM_MIN_TOKENS"), "120");
        assert_eq!(get("HEADROOM_MAX_ITEMS"), "8");
        assert_eq!(get("HEADROOM_SMART_CRUSHER_COMPACTION"), "0");
        assert_eq!(get("HEADROOM_FORCE_KOMPRESS"), "1");
        assert_eq!(get("HEADROOM_ACCURACY_GUARD"), "strict");
    }

    #[test]
    fn personas_omit_target_ratio() {
        for name in ["coding", "general"] {
            let profile = get_agent_savings_profile(name).unwrap();
            assert!(profile.target_ratio.is_none());
            assert!(!profile
                .proxy_env()
                .iter()
                .any(|(k, _)| *k == "HEADROOM_TARGET_RATIO"));
        }
    }

    #[test]
    fn unknown_profile_lists_valid_profiles() {
        let err = get_agent_savings_profile("missing").unwrap_err();
        assert_eq!(
            err,
            "unknown savings profile 'missing'; expected one of: agent-90, balanced, coding, general"
        );
    }

    #[test]
    fn profile_lookup_normalizes_and_defaults() {
        assert_eq!(
            get_agent_savings_profile(" Agent-90 ").unwrap().name,
            "agent-90"
        );
        assert_eq!(get_agent_savings_profile("").unwrap().name, "agent-90");
    }

    fn record(client: &str, before: i64, saved: i64) -> PerfRecord {
        PerfRecord {
            client: client.to_string(),
            tokens_before: before,
            tokens_saved: saved,
            ..Default::default()
        }
    }

    #[test]
    fn required_agents_each_meet_target() {
        let records = vec![
            record("claude", 1000, 920),
            record("codex", 1000, 910),
            record("cursor", 1000, 930),
        ];
        let required = split_required_agents("Claude, codex,cursor,");
        let messages = check_required_agents(&records, &required, 90.0).unwrap();
        assert_eq!(
            messages,
            vec![
                "claude: 92.0% savings meets 90.0% target",
                "codex: 91.0% savings meets 90.0% target",
                "cursor: 93.0% savings meets 90.0% target",
            ]
        );
    }

    #[test]
    fn required_agent_missing_fails() {
        let records = vec![record("claude", 1000, 920)];
        let required = split_required_agents("claude,codex,cursor");
        let err = check_required_agents(&records, &required, 90.0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "missing required agent traffic: codex, cursor"
        );
    }

    #[test]
    fn required_agent_below_target_fails() {
        let records = vec![record("claude", 1000, 500)];
        let required = split_required_agents("claude");
        let err = check_required_agents(&records, &required, 90.0).unwrap_err();
        assert_eq!(err.to_string(), "claude: 50.0% savings below 90.0% target");
    }

    #[test]
    fn accuracy_rate_reads_totals_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eval.json");
        std::fs::write(&path, r#"{"totals": {"accuracy_rate": 0.97}}"#).unwrap();
        assert_eq!(read_accuracy_rate(&path).unwrap(), 0.97);
        std::fs::write(&path, r#"{"accuracy_preservation_rate": "0.91"}"#).unwrap();
        assert_eq!(read_accuracy_rate(&path).unwrap(), 0.91);
        std::fs::write(&path, r#"{"totals": {}}"#).unwrap();
        let err = read_accuracy_rate(&path).unwrap_err().to_string();
        assert!(err.contains("does not contain totals.accuracy_rate"));
    }

    #[test]
    fn smoke_fixture_passes_real_gate() {
        let dir = tempfile::tempdir().unwrap();
        let eval_path = write_smoke_fixture(dir.path()).unwrap();
        assert!(eval_path.exists());
        assert_eq!(read_accuracy_rate(&eval_path).unwrap(), 1.0);
        // Env var mutation is process-wide; this is the only test in this
        // binary using HEADROOM_WORKSPACE_DIR.
        std::env::set_var("HEADROOM_WORKSPACE_DIR", dir.path());
        let report = headroom_core::perf_analyzer::parse_log_files(0.0);
        std::env::remove_var("HEADROOM_WORKSPACE_DIR");
        let clients: Vec<&str> = report
            .perf_records
            .iter()
            .map(|r| r.client.as_str())
            .collect();
        assert_eq!(clients, vec!["claude", "codex", "cursor"]);
        let messages = check_required_agents(
            &report.perf_records,
            &split_required_agents("claude,codex,cursor"),
            90.0,
        )
        .unwrap();
        assert_eq!(
            messages,
            vec![
                "claude: 92.0% savings meets 90.0% target",
                "codex: 91.0% savings meets 90.0% target",
                "cursor: 93.0% savings meets 90.0% target",
            ]
        );
    }
}
