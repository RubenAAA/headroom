//! Throwaway probe: the production lexical path, fed English queries.
//!
//! The counterpart to `memprobe_multilingual`. Same store copy, same
//! questions, but through `CtxStore::search` — the porter+trigram RRF the
//! proxy actually runs — with the Russian queries rendered in English by
//! hand. Answers whether a vector channel earns its latency, or whether
//! translating the query in front of what already exists is enough.
//!
//! Run: `cargo run -p headroom-core --release --example memprobe_lexical`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use headroom_core::ctx::{CtxStore, SearchOpts};

    // A copy of the copy: `CtxStore::open` writes WAL files and may migrate.
    let store = CtxStore::open("/tmp/memprobe_lex/memories_index.db")?;

    // The seven topic terms are already the English side of each pair.
    let terms = [
        "database", "proxy", "compression", "cache", "memory", "queries", "cohort",
    ];
    // Hand translations of the Russian sentences. These are the judgment call
    // in this comparison, so they are spelled out rather than computed.
    let sentences = [
        ("где лежит база данных с памятью", "where is the memory database stored"),
        (
            "почему прокси падает с ошибкой соединения",
            "why does the proxy fail with a connection error",
        ),
        ("как настроить сжатие контекста", "how to configure context compression"),
    ];

    let opts = SearchOpts {
        limit: 5,
        ..Default::default()
    };

    println!("=== single terms ===");
    for t in terms {
        let hits = store.search(&[t.to_string()], &opts)?;
        println!("\n{t}  ({} hits shown)", hits.len());
        for h in hits.iter().take(5) {
            println!("  {:>8.4} [{}] {}", h.rank, h.match_layer, trim(&h.content, 84));
        }
    }

    println!("\n=== sentences, translated ===");
    for (ru, en) in sentences {
        let hits = store.search(&[en.to_string()], &opts)?;
        println!("\n{ru}\n  -> \"{en}\"  ({} hits)", hits.len());
        for h in hits.iter().take(5) {
            println!("  {:>8.4} [{}] {}", h.rank, h.match_layer, trim(&h.content, 84));
        }
    }
    Ok(())
}

fn trim(t: &str, max: usize) -> String {
    let one_line: String = t.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let mut end = max.min(one_line.len());
    while end > 0 && !one_line.is_char_boundary(end) {
        end -= 1;
    }
    one_line[..end].to_string()
}
