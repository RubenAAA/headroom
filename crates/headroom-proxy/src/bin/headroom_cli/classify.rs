//! `headroom classify`: ask Jev (TypeSafe's System One model, served by
//! OpenCode Zen) typed questions about a piece of text. Request and response
//! shapes, limits and measured latency are in `docs/jev-model.md`.
//!
//! Direct from the CLI to Zen. It does not go through the proxy and sends no
//! `x-headroom-cwd`: the state leaves the machine, so it carries only what the
//! caller passed.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde_json::{Map, Value, json};

pub const DEFAULT_ENDPOINT: &str = "https://opencode.ai/zen/v1/systemone";
pub const DEFAULT_MODEL: &str = "jev-1.13-free";

type Error = Box<dyn std::error::Error>;

#[derive(Args)]
pub struct ClassifyArgs {
    /// Text to judge. Without this or --state-file, the state is read from stdin.
    #[arg(long, conflicts_with_all = ["state_file", "request"])]
    state: Option<String>,
    /// Read the text to judge from a file.
    #[arg(long, conflicts_with = "request")]
    state_file: Option<PathBuf>,
    /// A yes/no question; repeat for more (answered as q1, q2, ...).
    #[arg(long, conflicts_with = "request")]
    noul: Vec<String>,
    /// Full request JSON (`state`, `questions`, optional `model`) from a file,
    /// or `-` for stdin. The only way to ask `choice` and `score` questions.
    #[arg(long)]
    request: Option<String>,
    /// Model ID. `-free` IDs go without a key; others send OPENCODE_API_KEY.
    #[arg(long, default_value = DEFAULT_MODEL)]
    model: String,
    /// systemone endpoint.
    #[arg(long, env = "HEADROOM_JEV_URL", default_value = DEFAULT_ENDPOINT)]
    endpoint: String,
    /// Print the response JSON instead of one line per answer.
    #[arg(long = "json")]
    json: bool,
}

pub fn cmd_classify(args: ClassifyArgs) -> Result<(), Error> {
    let body = build_request(&args)?;
    let model = body["model"].as_str().unwrap_or(DEFAULT_MODEL);

    // The free ID rejects a key it does not recognise, so send one only where
    // the model is paid.
    let key = std::env::var("OPENCODE_API_KEY")
        .ok()
        .filter(|k| !k.is_empty() && !model.ends_with("-free"));

    let client = headroom_proxy::ssl_context::blocking_client_builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("headroom-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut req = client.post(&args.endpoint).json(&body);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        let shown: String = text.chars().take(500).collect();
        return Err(format!("jev: HTTP {status}: {shown}").into());
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "jev: reply is not JSON ({e}): {}",
            &text[..text.len().min(200)]
        )
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
    } else {
        println!("{}", render_text(&parsed, &body));
    }
    Ok(())
}

fn read_stdin() -> Result<String, Error> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s)
}

fn build_request(args: &ClassifyArgs) -> Result<Value, Error> {
    let mut body = if let Some(src) = &args.request {
        let raw = if src == "-" {
            read_stdin()?
        } else {
            std::fs::read_to_string(src)?
        };
        serde_json::from_str::<Value>(&raw)
            .map_err(|e| format!("--request is not valid JSON: {e}"))?
    } else {
        if args.noul.is_empty() {
            return Err("give at least one --noul question, or --request FILE".into());
        }
        let state = match (&args.state, &args.state_file) {
            (Some(s), _) => s.clone(),
            (None, Some(path)) => std::fs::read_to_string(path)?,
            (None, None) => read_stdin()?,
        };
        let questions: Map<String, Value> = args
            .noul
            .iter()
            .enumerate()
            .map(|(i, q)| {
                (
                    format!("q{}", i + 1),
                    json!({"type": "noul", "instructions": q}),
                )
            })
            .collect();
        json!({"state": state, "questions": questions})
    };

    let obj = body
        .as_object_mut()
        .ok_or("--request must be a JSON object")?;
    obj.entry("model").or_insert_with(|| json!(args.model));

    // Zen answers a missing `state` or empty `questions` with a 422 that says
    // "Endpoint is unavailable". Catch both here, where the message is true.
    let state_empty = match obj.get("state") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    };
    if state_empty {
        return Err("state is empty: pass --state, --state-file, stdin or a `state` field".into());
    }
    if obj
        .get("questions")
        .and_then(Value::as_object)
        .is_none_or(Map::is_empty)
    {
        return Err("`questions` must be a non-empty object".into());
    }
    Ok(body)
}

/// One line per answer, in the order the questions were asked. Zen returns
/// them in no fixed order. An answer the request did not name follows last.
fn render_text(resp: &Value, request: &Value) -> String {
    let Some(answers) = resp.get("answers").and_then(Value::as_object) else {
        return resp.to_string();
    };
    let asked = request["questions"].as_object();
    let in_order = asked.into_iter().flat_map(Map::keys);
    let unasked = answers
        .keys()
        .filter(|k| asked.is_none_or(|q| !q.contains_key(*k)));
    in_order
        .chain(unasked)
        .filter_map(|name| answers.get(name).map(|a| (name, a)))
        .map(|(name, a)| {
            let num = |k: &str| a.get(k).and_then(Value::as_f64);
            let conf = num("confidence")
                .map(|c| format!(" (confidence {c:.2})"))
                .unwrap_or_default();
            match a.get("type").and_then(Value::as_str) {
                Some("noul") => format!("{name}: {:.2}", num("noul").unwrap_or(f64::NAN)),
                Some("choice") => format!(
                    "{name}: {}{conf}",
                    a.get("choice").and_then(Value::as_str).unwrap_or("?")
                ),
                Some("score") => format!("{name}: {:.2}{conf}", num("score").unwrap_or(f64::NAN)),
                _ => format!("{name}: {a}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
