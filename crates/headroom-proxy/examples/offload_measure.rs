//! Scratch measurement for the Jev context-refresh idea: what does the real
//! ctx offload free on a captured Spark session at Spark-sized thresholds?
//! Usage: offload_measure <stage1.json>   (an array of Anthropic messages)
use headroom_proxy::compression::ctx_offload::{
    CtxOffloadConfig, OffloadGate, OffloadPolicy, offload_anthropic_request,
    offload_tool_use_inputs,
};
use serde_json::{Value, json};

const TAIL: usize = 20;

fn main() {
    let path = std::env::args().nth(1).expect("stage1.json");
    let all: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let keep = all.len().saturating_sub(TAIL);
    let size = |v: &Value| serde_json::to_string(v).unwrap().len();
    let base = size(&json!({"messages": &all[..keep]}));
    println!(
        "old messages {keep}, json {base} bytes (~{}k tokens at 4 B)",
        base / 4000
    );

    for min_bytes in [400usize, 1000, 2000, 4000, 20000] {
        let config = CtxOffloadConfig {
            min_bytes,
            stale_margin: 0,
            stale_window: 0,
            cross_session_seed: false,
        };
        let mut body = json!({"messages": &all[..keep]});
        let out = offload_anthropic_request(&mut body, &config, None);
        let after_results = size(&body);

        // tool_use inputs: a first conversion needs a rebuild boundary.
        let gate = OffloadGate::new(8);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "measure",
            rebuild_boundary: true,
        };
        let tu = offload_tool_use_inputs(&mut body, &config, &policy, None, &|_| true);
        let after_both = size(&body);
        println!(
            "min_bytes {min_bytes:>6}: tool_results offloaded {:>4} -> {:>7} B saved ({:.1}%); tool_use strings offloaded {:>4} -> total saved {:>7} B ({:.1}%), now ~{}k tokens",
            out.blocks_offloaded,
            base - after_results,
            100.0 * (base - after_results) as f64 / base as f64,
            tu.blocks_offloaded,
            base - after_both,
            100.0 * (base - after_both) as f64 / base as f64,
            after_both / 4000,
        );
        if min_bytes == 1000 {
            let stub: Vec<usize> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                .filter(|b| b["type"] == "tool_result")
                .map(|b| b["content"].to_string().len())
                .filter(|n| *n > 0)
                .collect();
            let mut s = stub;
            s.sort_unstable();
            println!(
                "  tool_result sizes after offload: median {} B, p90 {} B, max {} B",
                s[s.len() / 2],
                s[s.len() * 9 / 10],
                s[s.len() - 1]
            );
        }
    }
}
