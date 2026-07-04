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

use std::io::Read;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "headroom", about = "Headroom context-mode CLI")]
struct Cli {
    /// Proxy URL (default: http://127.0.0.1:8787).
    #[arg(long, env = "HEADROOM_PROXY_URL", default_value = "http://127.0.0.1:8787")]
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
            } => cmd_search(&cli.proxy_url, &query, &sort, source.as_deref(), r#type.as_deref()),
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
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
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

    let label = label
        .map(String::from)
        .unwrap_or_else(|| {
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
