//! Throwaway probe: the shipped memory search, against a copy of the store.
//!
//! Calls `CtxMemoryBackend::search_memories` — translation, interleave and
//! duplicate collapsing included — so the numbers come from the path that
//! serves requests rather than a hand-written stand-in.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use headroom_proxy::memory::backend::MemoryBackend;
    use headroom_proxy::memory::ctx_backend::CtxMemoryBackend;

    let backend = CtxMemoryBackend::open(std::path::Path::new("/tmp/memprobe_lex"))?;
    // The partition the live store keeps these under.
    let user = std::env::args().nth(1).unwrap_or_else(|| "default".into());

    for q in [
        "прокси",
        "кэш",
        "сжатие",
        "когорта",
        "почему прокси падает с ошибкой соединения",
        "как настроить сжатие контекста",
    ] {
        let hits = backend.search_memories(q, &user, 5, false).await?;
        println!("\n{q}  ({} hits)", hits.len());
        for h in &hits {
            println!("  {:.3} {}", h.score, trim(&h.memory.content, 84));
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
