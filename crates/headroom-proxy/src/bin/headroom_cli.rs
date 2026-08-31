//! `headroom` CLI — thin HTTP client for the context-mode endpoints.
//!
//! Usage:
//!   headroom ctx search "<query>" [--sort timeline] [--source S] [--type code|prose]
//!   headroom ctx get <hash>
//!   headroom ctx index <path>
//!   headroom ctx stats
//!
//! Requires the headroom-proxy to be running with `--ctx-offload`.
//! Proxy URL defaults to `http://127.0.0.1:8787`, overridable via
//! `--proxy-url` or `HEADROOM_PROXY_URL`.

use std::ffi::OsString;
use std::io::Read;

use clap::{Parser, Subcommand};

// Lives in a subdirectory so Cargo's bin auto-discovery doesn't treat it as
// its own binary target.
#[path = "headroom_cli/agent_savings.rs"]
mod agent_savings;
#[path = "headroom_cli/copilot_auth.rs"]
mod copilot_auth;
#[path = "headroom_cli/network_diff.rs"]
mod network_diff;
#[path = "headroom_cli/tools.rs"]
mod tools;

#[derive(Parser)]
#[command(name = "headroom", about = "Headroom context-mode CLI")]
struct Cli {
    /// Proxy URL (default: http://127.0.0.1:8787).
    #[arg(
        long,
        env = "HEADROOM_PROXY_URL",
        default_value = "http://127.0.0.1:8787"
    )]
    proxy_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Context-mode operations.
    Ctx {
        #[command(subcommand)]
        command: CtxCommand,
    },
    /// Render or verify Codex/Claude/Cursor token-savings settings.
    AgentSavings {
        /// Savings profile to render or check.
        #[arg(long, default_value = "agent-90")]
        profile: String,
        /// Output format for profile environment.
        #[arg(long = "format", default_value = "shell", value_parser = ["shell", "json"])]
        output_format: String,
        /// Check recent proxy logs against the profile savings target.
        #[arg(long)]
        check_perf: bool,
        /// Hours of proxy logs to inspect with --check-perf (0 = all data).
        #[arg(long, default_value_t = 24.0)]
        hours: f64,
        /// Headroom eval JSON report proving accuracy preservation.
        #[arg(long)]
        accuracy_report: Option<std::path::PathBuf>,
        /// Write deterministic three-agent PERF/eval fixture into workspace dir.
        #[arg(long)]
        write_smoke_fixture: Option<std::path::PathBuf>,
        /// Comma-separated clients that must each meet the savings target.
        #[arg(long, default_value = "")]
        require_agents: String,
        /// Minimum accepted accuracy preservation rate.
        #[arg(long, default_value_t = 0.90)]
        min_accuracy: f64,
    },
    /// Capture and compare network traffic for Headroom investigations.
    Capture {
        #[command(subcommand)]
        command: CaptureCommand,
    },
    /// Manage Headroom's GitHub Copilot OAuth token.
    CopilotAuth {
        #[command(subcommand)]
        command: CopilotAuthCommand,
    },
    /// Show estimated/measured output-token reduction from the shaper.
    OutputSavings,
    /// Analyze proxy performance from logs.
    Perf {
        /// Analyze logs from the last N hours (default: 168 = 7 days; 0 = all data).
        #[arg(long, default_value_t = 168.0)]
        hours: f64,
        /// Show raw PERF records instead of report.
        #[arg(long)]
        raw: bool,
        /// Output format (default: text). json/csv emit machine-readable data.
        #[arg(long = "format", default_value = "text", value_parser = ["text", "json", "csv"])]
        output_format: String,
    },
    /// Run a reduced Rust health check for proxy liveness and local ledgers.
    Doctor {
        /// Emit JSON instead of text.
        #[arg(long = "json")]
        json: bool,
    },
    /// Show durable compression savings over time.
    Savings {
        /// Emit the raw report as JSON.
        #[arg(long = "json")]
        as_json: bool,
        /// Retention/lookback window for the ledger, in days.
        #[arg(long, default_value_t = default_days(), value_parser = clap::value_parser!(u32).range(1..))]
        days: u32,
        /// Delete the savings ledger and start fresh.
        #[arg(long)]
        reset: bool,
    },
    /// Run ast-grep (AST-aware structural search/replace).
    #[command(name = "sg", disable_help_flag = true)]
    Sg {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run difftastic (structural diff).
    #[command(name = "diff", disable_help_flag = true)]
    Diff {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run scc (fast lines-of-code / repo-shape probe).
    #[command(name = "loc", disable_help_flag = true)]
    Loc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Manage bundled CLI tool binaries.
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
}

#[derive(Subcommand)]
enum CaptureCommand {
    /// Compare direct and Headroom MITM capture JSONL files.
    NetworkDiff {
        /// JSONL capture from the direct Claude Code lane.
        #[arg(long)]
        direct: std::path::PathBuf,
        /// JSONL capture from the Headroom-proxied Claude Code lane.
        #[arg(long)]
        headroom: std::path::PathBuf,
        /// Write a Markdown report to this path. Defaults to stdout.
        #[arg(long)]
        output: Option<std::path::PathBuf>,
        /// Optional machine-readable JSON diff output.
        #[arg(long)]
        json_output: Option<std::path::PathBuf>,
        /// Pair exchanges by method+path or by method+host+path.
        #[arg(long, default_value = "path", value_parser = ["path", "route"])]
        pair_by: String,
    },
}

#[derive(Subcommand)]
enum CopilotAuthCommand {
    /// Sign in with GitHub's Copilot OAuth device-code flow.
    Login {
        /// GitHub login domain. Use github.com for GitHub.com Enterprise
        /// Cloud; only pass a custom hostname for GitHub Enterprise Server.
        #[arg(long, default_value = copilot_auth::DEFAULT_GITHUB_HOST)]
        domain: String,
    },
    /// Show whether Headroom has a saved Copilot OAuth token.
    Status,
}

#[derive(Subcommand)]
enum ToolsCommand {
    /// Print the tool registry.
    List,
    /// Check the status of every bundled tool.
    Doctor {
        /// Emit JSON instead of a table.
        #[arg(long = "json")]
        json: bool,
    },
    /// Pre-fetch bundled tool binaries into the per-user cache.
    Install {
        /// Install only the named tool (repeatable). Default: all.
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// Re-fetch even if the binary is already cached.
        #[arg(long)]
        force: bool,
    },
}

fn default_days() -> u32 {
    headroom_core::savings_ledger::DEFAULT_RETENTION_DAYS as u32
}

#[derive(Subcommand)]
enum CtxCommand {
    /// Search the content index.
    Search {
        /// Search query.
        query: String,
        /// Sort mode: relevance (default) or timeline.
        #[arg(long, default_value = "relevance")]
        sort: String,
        /// Source label filter.
        #[arg(long)]
        source: Option<String>,
        /// Content type: code or prose.
        #[arg(long)]
        r#type: Option<String>,
    },
    /// Retrieve an offloaded original by hash.
    Get {
        /// The blake3 hash (24 hex chars).
        hash: String,
    },
    /// Index content into the search store.
    Index {
        /// File path to read and index. Use "-" for stdin.
        path: String,
        /// Label for the indexed content (default: filename).
        #[arg(long)]
        label: Option<String>,
    },
    /// Fetch a URL, convert to markdown, and index into the search store.
    Fetch {
        /// URL to fetch.
        url: String,
        /// Label for the indexed content.
        #[arg(long)]
        source: Option<String>,
        /// Skip cache and re-fetch.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Cache TTL in seconds (default 86400 = 24h).
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Show offload/search statistics.
    Stats,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Ctx { command: ctx } => match ctx {
            CtxCommand::Search {
                query,
                sort,
                source,
                r#type,
            } => cmd_search(
                &cli.proxy_url,
                &query,
                &sort,
                source.as_deref(),
                r#type.as_deref(),
            ),
            CtxCommand::Get { hash } => cmd_get(&cli.proxy_url, &hash),
            CtxCommand::Index { path, label } => cmd_index(&cli.proxy_url, &path, label.as_deref()),
            CtxCommand::Fetch {
                url,
                source,
                force,
                ttl,
            } => cmd_fetch(&cli.proxy_url, &url, source.as_deref(), force, ttl),
            CtxCommand::Stats => cmd_stats(&cli.proxy_url),
        },
        Command::AgentSavings {
            profile,
            output_format,
            check_perf,
            hours,
            accuracy_report,
            write_smoke_fixture,
            require_agents,
            min_accuracy,
        } => cmd_agent_savings(
            &profile,
            &output_format,
            check_perf,
            hours,
            accuracy_report.as_deref(),
            write_smoke_fixture.as_deref(),
            &require_agents,
            min_accuracy,
        ),
        Command::Capture { command } => match command {
            CaptureCommand::NetworkDiff {
                direct,
                headroom,
                output,
                json_output,
                pair_by,
            } => cmd_network_diff(
                &direct,
                &headroom,
                output.as_deref(),
                json_output.as_deref(),
                &pair_by,
            ),
        },
        Command::CopilotAuth { command } => match command {
            CopilotAuthCommand::Login { domain } => cmd_copilot_login(&domain),
            CopilotAuthCommand::Status => cmd_copilot_status(),
        },
        Command::OutputSavings => cmd_output_savings(),
        Command::Perf {
            hours,
            raw,
            output_format,
        } => cmd_perf(hours, raw, &output_format),
        Command::Doctor { json } => cmd_doctor(&cli.proxy_url, json),
        Command::Savings {
            as_json,
            days,
            reset,
        } => cmd_savings(as_json, days, reset),
        Command::Sg { args } => tools::exec_tool("ast-grep", args),
        Command::Diff { args } => tools::exec_tool("difft", args),
        Command::Loc { args } => tools::exec_tool("scc", args),
        Command::Tools { command } => match command {
            ToolsCommand::List => tools::cmd_list(),
            ToolsCommand::Doctor { json } => match tools::cmd_doctor(json) {
                Ok(code) if code != 0 => std::process::exit(code),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            },
            ToolsCommand::Install { tools, force } => match tools::cmd_install(tools, force) {
                Ok(code) if code != 0 => std::process::exit(code),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            },
        },
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn client() -> reqwest::blocking::Client {
    // The ctx stores are sharded per project, and the proxy reads the project
    // off this header. Sending the CLI's own working directory is what makes
    // `headroom ctx search` search the project it was run from rather than the
    // shared bucket.
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(cwd) = std::env::current_dir()
        .ok()
        .and_then(|d| reqwest::header::HeaderValue::from_str(&d.to_string_lossy()).ok())
    {
        headers.insert("x-headroom-cwd", cwd);
    }
    headroom_proxy::ssl_context::blocking_client_builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client")
}

fn cmd_search(
    base: &str,
    query: &str,
    sort: &str,
    source: Option<&str>,
    content_type: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!("{base}/ctx/search?q={}", urlencoding::encode(query));
    url.push_str(&format!("&sort={sort}"));
    if let Some(s) = source {
        url.push_str(&format!("&source={}", urlencoding::encode(s)));
    }
    if let Some(t) = content_type {
        url.push_str(&format!("&type={t}"));
    }

    let resp = client().get(&url).send()?;
    if !resp.status().is_success() {
        eprintln!("search failed: HTTP {}", resp.status());
        std::process::exit(1);
    }
    let body: serde_json::Value = resp.json()?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

fn cmd_get(base: &str, hash: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{base}/ctx/get/{hash}");
    let resp = client().get(&url).send()?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        eprintln!("not found: {hash}");
        std::process::exit(1);
    }
    if !resp.status().is_success() {
        eprintln!("get failed: HTTP {}", resp.status());
        std::process::exit(1);
    }

    let body: serde_json::Value = resp.json()?;
    // Print the content directly (not the wrapper JSON) for piping.
    if let Some(content) = body.get("content").and_then(|v| v.as_str()) {
        print!("{content}");
    } else {
        println!("{}", serde_json::to_string_pretty(&body)?);
    }
    Ok(())
}

fn cmd_index(
    base: &str,
    path: &str,
    label: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = if path == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(path)?
    };

    let label = label.map(String::from).unwrap_or_else(|| {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stdin".to_string())
    });

    let body = serde_json::json!({
        "label": label,
        "content": content,
    });

    let resp = client()
        .post(format!("{base}/ctx/index"))
        .json(&body)
        .send()?;

    if !resp.status().is_success() {
        eprintln!("index failed: HTTP {}", resp.status());
        std::process::exit(1);
    }
    let result: serde_json::Value = resp.json()?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn cmd_fetch(
    base: &str,
    url: &str,
    source: Option<&str>,
    force: bool,
    ttl: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "url": url,
        "source": source,
        "force": force,
        "ttl": ttl,
    });

    let resp = client()
        .post(format!("{base}/ctx/fetch"))
        .json(&body)
        .send()?;

    if !resp.status().is_success() {
        eprintln!("fetch failed: HTTP {}", resp.status());
        let text = resp.text().unwrap_or_default();
        eprintln!("{text}");
        std::process::exit(1);
    }
    let result: serde_json::Value = resp.json()?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn cmd_stats(base: &str) -> Result<(), Box<dyn std::error::Error>> {
    let resp = client().get(format!("{base}/ctx/stats")).send()?;
    if !resp.status().is_success() {
        eprintln!("stats failed: HTTP {}", resp.status());
        std::process::exit(1);
    }
    let body: serde_json::Value = resp.json()?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// `headroom copilot-auth` — GitHub Copilot OAuth token management.
// ---------------------------------------------------------------------------

fn cmd_copilot_login(domain: &str) -> Result<(), Box<dyn std::error::Error>> {
    let device = copilot_auth::start_device_authorization(domain)
        .map_err(|e| format!("Unable to start GitHub device login: {e}"))?;

    println!("GitHub Copilot OAuth login");
    println!("  Open: {}", device.verification_uri);
    println!("  Code: {}", device.user_code);
    println!("  Waiting for authorization...");

    let token = copilot_auth::poll_device_authorization(
        &device.device_code,
        domain,
        device.interval,
        device.expires_in,
    )
    .map_err(|e| format!("GitHub device login failed: {e}"))?;

    let path = copilot_auth::save_oauth_token(&token, domain)?;
    println!("  Saved: {}", path.display());
    println!(
        "  Token fingerprint: {}",
        copilot_auth::token_fingerprint(&token)
    );
    Ok(())
}

fn cmd_copilot_status() -> Result<(), Box<dyn std::error::Error>> {
    let token = copilot_auth::read_oauth_token();
    println!("Auth file: {}", copilot_auth::auth_path().display());
    match token {
        None => println!("Status: not logged in"),
        Some(token) => {
            println!("Status: logged in");
            println!(
                "Token fingerprint: {}",
                copilot_auth::token_fingerprint(&token)
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `headroom output-savings` — counterfactual output-token reduction report.
//
// Reads the shaper's savings ledger directly from disk (no proxy needed) via
// `headroom_core::output_savings`, mirroring the Python `headroom
// output-savings` click command's output format.
// ---------------------------------------------------------------------------

fn cmd_output_savings() -> Result<(), Box<dyn std::error::Error>> {
    use headroom_core::output_savings::SavingsLedger;

    let path = headroom_core::paths::output_savings_path();
    if !path.exists() {
        println!("No output-savings data yet.");
        println!("Run `headroom learn --verbosity --apply` to seed the baseline,");
        println!("then enable the shaper (HEADROOM_OUTPUT_SHAPER=1) and send traffic.");
        return Ok(());
    }

    let ledger = SavingsLedger::load(&path);
    print!("{}", format_output_savings(&ledger));
    Ok(())
}

fn format_output_savings(ledger: &headroom_core::output_savings::SavingsLedger) -> String {
    let est = ledger.best_estimate();
    let mut out = String::new();
    out.push_str(&format!("\n{}\n", "=".repeat(56)));
    out.push_str("Output-token reduction\n");
    out.push_str(&format!("{}\n", "=".repeat(56)));
    if est.n_requests == 0 {
        out.push_str("  No shaped requests recorded yet.\n");
        out.push_str(&format!(
            "  Baseline: {} samples, {} strata.\n",
            ledger.baseline.total_samples(),
            ledger.baseline.strata.len()
        ));
        return out;
    }

    let label = if est.kind == "measured" {
        "MEASURED (A/B holdout)"
    } else {
        "ESTIMATED (synthetic control)"
    };
    out.push_str(&format!("  Method:    {label}\n"));
    out.push_str(&format!(
        "  Requests:  {} shaped\n",
        commafy(est.n_requests)
    ));
    out.push_str(&format!(
        "  Baseline:  {} output tokens expected\n",
        commafy(est.baseline_tokens.round_ties_even() as i64)
    ));
    out.push_str(&format!(
        "  Saved:     {} output tokens\n",
        commafy(est.tokens_saved.round_ties_even() as i64)
    ));
    out.push_str(&format!(
        "  Reduction: {:.1}%   (95% CI {:.1}% … {:.1}%)\n",
        est.pct, est.ci_low_pct, est.ci_high_pct
    ));
    if est.kind == "estimated" {
        out.push_str(
            "\n  Note: estimated vs the learned baseline. For a measured number,\
             \n  set HEADROOM_OUTPUT_HOLDOUT=0.1 to leave 10% of conversations\
             \n  unshaped as a control arm.\n",
        );
    }
    out
}

// ---------------------------------------------------------------------------
// `headroom capture network-diff` — compare direct vs Headroom captures.
// ---------------------------------------------------------------------------

fn cmd_network_diff(
    direct_path: &std::path::Path,
    headroom_path: &std::path::Path,
    markdown_output: Option<&std::path::Path>,
    json_output: Option<&std::path::Path>,
    pair_by: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let direct = network_diff::load_capture_file(direct_path, "direct")?;
    let headroom = network_diff::load_capture_file(headroom_path, "headroom")?;
    let diff = network_diff::compare_captures(&direct, &headroom, pair_by);
    let markdown = network_diff::render_markdown_report(&diff);

    if let Some(path) = markdown_output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &markdown)?;
        println!("Wrote Markdown report: {}", path.display());
    } else {
        println!("{markdown}");
    }

    if let Some(path) = json_output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&diff.to_dict())?)?;
        println!("Wrote JSON report: {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `headroom agent-savings` — render or verify token-savings profiles.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_agent_savings(
    profile: &str,
    output_format: &str,
    check_perf: bool,
    hours: f64,
    accuracy_report: Option<&std::path::Path>,
    write_smoke_fixture: Option<&std::path::Path>,
    require_agents: &str,
    min_accuracy: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    use headroom_core::perf_analyzer as perf;

    let savings_profile = agent_savings::get_agent_savings_profile(profile)?;
    if hours < 0.0 {
        return Err("--hours must be >= 0".into());
    }

    if let Some(workspace) = write_smoke_fixture {
        let eval_path = agent_savings::write_smoke_fixture(workspace)?;
        println!("Wrote agent-90 smoke fixture to {}", workspace.display());
        println!(
            "Verify with: HEADROOM_WORKSPACE_DIR={} headroom agent-savings --check-perf \
             --hours 0 --require-agents claude,codex,cursor --accuracy-report {}",
            workspace.display(),
            eval_path.display()
        );
        return Ok(());
    }

    if check_perf || accuracy_report.is_some() {
        let mut messages: Vec<String> = Vec::new();

        if check_perf {
            let report = perf::parse_log_files(hours);
            let summary = perf::build_perf_summary(&report, None);
            let measured = summary
                .get("savings_pct")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let target = savings_profile.target_savings * 100.0;
            if measured < target {
                return Err(format!(
                    "{measured:.1}% savings below {target:.1}% target for {}",
                    savings_profile.name
                )
                .into());
            }
            messages.push(format!(
                "{measured:.1}% savings meets {target:.1}% target for {}",
                savings_profile.name
            ));
            let required = agent_savings::split_required_agents(require_agents);
            if !required.is_empty() {
                messages.extend(agent_savings::check_required_agents(
                    &report.perf_records,
                    &required,
                    target,
                )?);
            }
        }

        if let Some(path) = accuracy_report {
            let accuracy = agent_savings::read_accuracy_rate(path)?;
            if accuracy < min_accuracy {
                return Err(format!(
                    "{:.1}% accuracy below {:.1}% target",
                    accuracy * 100.0,
                    min_accuracy * 100.0
                )
                .into());
            }
            messages.push(format!(
                "{:.1}% accuracy meets {:.1}% target",
                accuracy * 100.0,
                min_accuracy * 100.0
            ));
        }

        println!("{}", messages.join("\n"));
        return Ok(());
    }

    let env = savings_profile.proxy_env();
    if output_format == "json" {
        // Python emits json.dumps(env, indent=2, sort_keys=True).
        let sorted: std::collections::BTreeMap<&str, &str> =
            env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        println!("{}", serde_json::to_string_pretty(&sorted)?);
        return Ok(());
    }
    for (key, value) in env {
        println!("export {key}={}", serde_json::to_string(&value)?);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `headroom perf` — proxy performance analysis from logs.
// ---------------------------------------------------------------------------

fn cmd_perf(hours: f64, raw: bool, output_format: &str) -> Result<(), Box<dyn std::error::Error>> {
    use headroom_core::perf_analyzer as perf;

    if hours < 0.0 {
        return Err("--hours must be >= 0".into());
    }
    let report = perf::parse_log_files(hours);
    let cli_stats = perf::context_tool_lifetime_savings();

    if output_format == "json" {
        let payload = if raw {
            serde_json::to_value(&report.perf_records)?
        } else {
            perf::build_perf_summary(&report, cli_stats.as_ref())
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if output_format == "csv" {
        print!("{}", perf_csv(&report, raw)?);
        return Ok(());
    }

    if raw {
        for r in &report.perf_records {
            println!(
                "{} {} model={} msgs={} before={} after={} saved={} cache_read={} \
                 cache_write={} cache_hit={}% opt={:.0}ms",
                r.timestamp,
                r.request_id,
                r.model,
                r.num_messages,
                r.tokens_before,
                r.tokens_after,
                r.tokens_saved,
                r.cache_read,
                r.cache_write,
                r.cache_hit_pct,
                r.optimization_ms
            );
        }
        if report.perf_records.is_empty() {
            println!("No PERF records found. Run the proxy first: headroom proxy");
        }
    } else {
        println!("{}", perf::format_report(&report, cli_stats.as_ref()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `headroom doctor` — reduced Rust health check.
//
// The full Python doctor correlates installer manifests and per-client wrap
// config. That surface depends on the install stack; this Rust command keeps
// the Tier 1/2 checks local and explicit.
// ---------------------------------------------------------------------------

fn proxy_health_urls(proxy_url: &str) -> [String; 2] {
    let base = proxy_url.trim_end_matches('/');
    // The Rust proxy owns /healthz. Keep /livez as a 404-only fallback for a
    // direct legacy Python proxy, where it remains the liveness endpoint.
    [format!("{base}/healthz"), format!("{base}/livez")]
}

fn probe_proxy_health(proxy_url: &str) -> serde_json::Value {
    let urls = proxy_health_urls(proxy_url);
    for (index, url) in urls.iter().enumerate() {
        match client().get(url).send() {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 404 && index + 1 < urls.len() {
                    continue;
                }
                return serde_json::json!({
                    "url": url,
                    "ok": status.is_success(),
                    "status": status.as_u16(),
                    "endpoint": if index == 0 { "healthz" } else { "livez" },
                });
            }
            Err(e) => {
                return serde_json::json!({
                    "url": url,
                    "ok": false,
                    "endpoint": if index == 0 { "healthz" } else { "livez" },
                    "error": e.to_string(),
                });
            }
        }
    }
    unreachable!("the health probe always has a primary endpoint")
}

fn cmd_doctor(proxy_url: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = probe_proxy_health(proxy_url);
    let path_status = |name: &str, path: std::path::PathBuf| {
        serde_json::json!({
            "name": name,
            "path": path.to_string_lossy(),
            "exists": path.exists(),
        })
    };
    let checks = serde_json::json!({
        "proxy_health": proxy,
        "paths": [
            path_status("workspace", headroom_core::paths::workspace_dir()),
            path_status("config", headroom_core::paths::config_dir()),
            path_status("savings_events", headroom_core::paths::savings_events_path(None)),
            path_status("output_savings", headroom_core::paths::output_savings_path()),
            path_status("copilot_auth", headroom_core::paths::copilot_auth_path()),
        ],
        "scope": "reduced-rust",
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        let proxy = &checks["proxy_health"];
        println!("Headroom doctor (reduced Rust)");
        println!(
            "proxy health: {} ({})",
            if proxy["ok"].as_bool().unwrap_or(false) {
                "ok"
            } else {
                "not ok"
            },
            proxy["url"].as_str().unwrap_or("")
        );
        if let Some(error) = proxy.get("error").and_then(|v| v.as_str()) {
            println!("  error: {error}");
        }
        println!("paths:");
        for row in checks["paths"].as_array().into_iter().flatten() {
            println!(
                "  {:<15} {} {}",
                row["name"].as_str().unwrap_or(""),
                if row["exists"].as_bool().unwrap_or(false) {
                    "exists "
                } else {
                    "missing"
                },
                row["path"].as_str().unwrap_or("")
            );
        }
    }
    let ok = checks["proxy_health"]["ok"].as_bool().unwrap_or(false);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// CSV field quoting per RFC 4180 (matches Python's csv module output).
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn perf_csv(
    report: &headroom_core::perf_analyzer::PerfReport,
    raw: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut out = String::new();
    if raw {
        out.push_str(&headroom_core::perf_analyzer::PERF_RECORD_FIELDS.join(","));
        out.push_str("\r\n");
        for r in &report.perf_records {
            let stages = serde_json::to_string(&r.stages)?;
            let row = [
                r.timestamp.clone(),
                r.request_id.clone(),
                r.model.clone(),
                r.client.clone(),
                r.num_messages.to_string(),
                r.tokens_before.to_string(),
                r.tokens_after.to_string(),
                r.tokens_saved.to_string(),
                r.cache_read.to_string(),
                r.cache_write.to_string(),
                r.cache_hit_pct.to_string(),
                r.optimization_ms.to_string(),
                r.transforms.join(","),
                r.total_ms.to_string(),
                r.tokens_out.to_string(),
                r.ttfb_ms.to_string(),
                stages,
            ];
            let cells: Vec<String> = row.iter().map(|c| csv_field(c)).collect();
            out.push_str(&cells.join(","));
            out.push_str("\r\n");
        }
    } else {
        // Non-raw CSV is the per-model breakdown.
        let summary = headroom_core::perf_analyzer::build_perf_summary(report, None);
        let fields = [
            "model",
            "requests",
            "tokens_before",
            "tokens_after",
            "tokens_saved",
            "savings_pct",
            "list_price_per_mtok",
        ];
        out.push_str(&fields.join(","));
        out.push_str("\r\n");
        if let Some(rows) = summary.get("by_model").and_then(|v| v.as_array()) {
            for row in rows {
                let cells: Vec<String> = fields
                    .iter()
                    .map(|f| match row.get(*f) {
                        Some(serde_json::Value::String(s)) => csv_field(s),
                        Some(serde_json::Value::Null) | None => String::new(),
                        Some(v) => v.to_string(),
                    })
                    .collect();
                out.push_str(&cells.join(","));
                out.push_str("\r\n");
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// `headroom savings` — durable compression-savings report.
//
// Reads the append-only savings ledger directly from disk (no proxy needed;
// aggregated on read) via `headroom_core::savings_ledger`, mirroring the
// Python `headroom savings` click command's flags and output format.
// ---------------------------------------------------------------------------

const BAR_WIDTH: usize = 16;

fn cmd_savings(as_json: bool, days: u32, reset: bool) -> Result<(), Box<dyn std::error::Error>> {
    use headroom_core::savings_ledger as ledger;

    if reset {
        let path = headroom_core::paths::savings_events_path(None);
        if path.exists() {
            std::fs::remove_file(&path)?;
            println!("Ledger reset: {}", path.display());
        } else {
            println!("Nothing to reset — ledger does not exist.");
        }
        return Ok(());
    }

    let report = ledger::aggregate_savings(None, None, days as i64);

    if as_json {
        println!("{}", serde_json::to_string_pretty(&report.to_value())?);
        return Ok(());
    }

    let calls = report
        .lifetime
        .get("calls")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if calls == 0 {
        println!("No savings recorded yet.");
        println!(
            "Compress via the Headroom MCP tool or route traffic through the \
             proxy, then re-run `headroom savings`."
        );
        println!("Ledger: {}", report.path);
        return Ok(());
    }

    println!();
    println!("Compression reduction on saving turns");
    println!("Scope: pre-compression input selected by transforms; not all provider input.");
    println!("{}", window_line("Today", &report.windows["today"]));
    println!(
        "{}",
        window_line("Last 7 days", &report.windows["last_7_days"])
    );
    // The ledger is capped at `MAX_RETENTION_DAYS`, so no all-time total
    // exists to print. Asking for one returned null, which rendered as a
    // "0 / 0 tokens" row underneath two populated ones.
    let span = (days as i64).min(headroom_core::savings_ledger::MAX_RETENTION_DAYS);
    println!(
        "{}",
        window_line(
            &format!("Last {span} days"),
            &report.windows["last_30_days"]
        )
    );

    if !report.by_model.is_empty() {
        println!();
        println!("Estimated cost avoided per model:");
        for row in &report.by_model {
            let model = row
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let cost = row.get("cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
            println!("  {:<24} {}", model, fmt_money(cost, 4));
        }
        println!("  Legacy note: rows without cost_basis assumed fresh-input pricing;");
        println!("  newer proxy rows use the measured fresh/cache-read placement.");
    }

    if !report.by_client.is_empty() {
        println!();
        println!("Savings by client:");
        for row in &report.by_client {
            let cl = row
                .get("client")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let n = row.get("calls").and_then(|v| v.as_i64()).unwrap_or(0);
            let saved = row
                .get("tokens_saved")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            println!(
                "  {:<24} {} calls · {} tokens saved",
                cl,
                commafy(n),
                commafy(saved)
            );
        }
    }

    Ok(())
}

fn window_line(label: &str, window: &serde_json::Value) -> String {
    let pct = window
        .get("savings_percent")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let saved = window
        .get("tokens_saved")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let before = window
        .get("tokens_before")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cost = window
        .get("cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    format!(
        "{:<11} {} {:>5.1}%  saved {} / {} selected tokens  {}",
        label,
        bar(pct),
        pct,
        commafy(saved),
        commafy(before),
        fmt_money(cost, 4)
    )
}

fn bar(percent: f64) -> String {
    let filled = (percent / 100.0 * BAR_WIDTH as f64).round_ties_even() as i64;
    let filled = filled.clamp(0, BAR_WIDTH as i64) as usize;
    let mut s = String::new();
    for _ in 0..filled {
        s.push('█');
    }
    for _ in 0..(BAR_WIDTH - filled) {
        s.push('░');
    }
    s
}

/// Format an integer with `,` thousands separators (mirrors Python `{:,}`).
fn commafy(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let grouped = group_thousands(&digits);
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Format a monetary value as `$` + thousands-separated integer part + fixed
/// decimals (mirrors Python `f"${value:,.Nf}"`).
fn fmt_money(value: f64, places: usize) -> String {
    let formatted = format!("{:.*}", places, value.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (formatted, None),
    };
    let grouped = group_thousands(&int_part);
    let sign = if value.is_sign_negative() && value != 0.0 {
        "-"
    } else {
        ""
    };
    match frac_part {
        Some(f) => format!("{sign}${grouped}.{f}"),
        None => format!("{sign}${grouped}"),
    }
}

fn group_thousands(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(bar(0.0), "░".repeat(16));
        assert_eq!(bar(100.0), "█".repeat(16));
        // 50% -> 8 filled
        assert_eq!(bar(50.0), format!("{}{}", "█".repeat(8), "░".repeat(8)));
        // clamp above 100
        assert_eq!(bar(150.0), "█".repeat(16));
    }

    #[test]
    fn commafy_groups_thousands() {
        assert_eq!(commafy(0), "0");
        assert_eq!(commafy(999), "999");
        assert_eq!(commafy(1_000), "1,000");
        assert_eq!(commafy(1_234_567), "1,234,567");
        assert_eq!(commafy(-12_345), "-12,345");
    }

    #[test]
    fn fmt_money_matches_python_format() {
        assert_eq!(fmt_money(0.0, 4), "$0.0000");
        assert_eq!(fmt_money(1234.5, 4), "$1,234.5000");
        assert_eq!(fmt_money(0.0018, 4), "$0.0018");
        assert_eq!(fmt_money(1_000_000.0, 2), "$1,000,000.00");
    }

    #[test]
    fn output_savings_empty_ledger_shows_baseline_summary() {
        use headroom_core::output_savings::SavingsLedger;
        let mut ledger = SavingsLedger::default();
        ledger.baseline.observe("sonnet|c|s|tools", 100);
        ledger.baseline.observe("sonnet|c|s|tools", 200);
        ledger.baseline.observe("gpt|c|m|notools", 50);
        let out = format_output_savings(&ledger);
        assert!(out.contains("Output-token reduction"));
        assert!(out.contains("No shaped requests recorded yet."));
        assert!(out.contains("Baseline: 3 samples, 2 strata."));
    }

    #[test]
    fn output_savings_estimated_report() {
        use headroom_core::output_savings::SavingsLedger;
        let mut ledger = SavingsLedger::default();
        // Baseline mean 100 over many samples; 30 shaped requests at 70.
        for _ in 0..50 {
            ledger.baseline.observe("k", 100);
        }
        for _ in 0..30 {
            ledger.record("treatment", "k", 70);
        }
        let out = format_output_savings(&ledger);
        assert!(out.contains("Method:    ESTIMATED (synthetic control)"));
        assert!(out.contains("Requests:  30 shaped"));
        assert!(out.contains("Baseline:  3,000 output tokens expected"));
        assert!(out.contains("Saved:     900 output tokens"));
        assert!(out.contains("Reduction: 30.0%"));
        assert!(out.contains("HEADROOM_OUTPUT_HOLDOUT=0.1"));
    }

    #[test]
    fn output_savings_measured_report_has_no_note() {
        use headroom_core::output_savings::SavingsLedger;
        let mut ledger = SavingsLedger::default();
        for _ in 0..10 {
            ledger.record("control", "k", 100);
            ledger.record("treatment", "k", 70);
        }
        let out = format_output_savings(&ledger);
        assert!(out.contains("Method:    MEASURED (A/B holdout)"));
        assert!(out.contains("Reduction: 30.0%"));
        assert!(!out.contains("HEADROOM_OUTPUT_HOLDOUT"));
    }

    #[test]
    fn window_line_layout() {
        let w = json!({
            "savings_percent": 50.0,
            "tokens_saved": 500,
            "tokens_before": 1000,
            "cost_usd": 0.0018,
        });
        let line = window_line("Today", &w);
        assert!(line.starts_with("Today      ")); // label left-justified width 11
        assert!(line.contains(" 50.0%"));
        assert!(line.contains("saved 500 / 1,000 selected tokens"));
        assert!(line.contains("$0.0018"));
    }

    #[test]
    fn doctor_prefers_the_rust_health_endpoint_with_legacy_fallback() {
        assert_eq!(
            proxy_health_urls("http://127.0.0.1:8787/"),
            [
                "http://127.0.0.1:8787/healthz".to_string(),
                "http://127.0.0.1:8787/livez".to_string(),
            ]
        );
    }
}
