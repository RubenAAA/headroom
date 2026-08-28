//! Throwaway probe: does a multilingual embedder retrieve across scripts?
//!
//! Reads a *copy* of the memory index (`/tmp/memprobe/memories_index.db`),
//! embeds every chunk with `MultilingualE5Small`, and runs the same seven
//! paired EN/RU queries the FTS baseline was measured on, plus a few
//! full-sentence Russian questions of the kind that actually arrive. Prints
//! what comes back; touches nothing in the live path.
//!
//! Run: `cargo run -p headroom-core --release --example memprobe_multilingual`

#[cfg(not(feature = "ml"))]
fn main() {
    eprintln!("needs --features ml");
}

#[cfg(feature = "ml")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

    const DB: &str = "/tmp/memprobe/memories_index.db";
    // Which model to measure. The E5 sizes come from fastembed's catalog;
    // `user2` is deepvk/USER2-small, a 34M bilingual ru-en retriever that
    // fastembed does not ship, so it loads as a user-defined ONNX model from
    // `USER2_DIR`. Each family wants its own prefixes.
    let which = std::env::var("E5").unwrap_or_else(|_| "small".into());
    println!("model: {which}");
    const TOP_K: usize = 10;
    const SHOW: usize = 5;

    // Seven topics, each an English term and its Russian counterpart. Same
    // list the FTS baseline used, so the two are comparable.
    let pairs: [(&str, &str); 7] = [
        ("database", "база данных"),
        ("proxy", "прокси"),
        ("compression", "сжатие"),
        ("cache", "кэш"),
        ("memory", "память"),
        ("queries", "запросы"),
        ("cohort", "когорта"),
    ];

    // Chunks are the unit the FTS tables index and the unit recall is served
    // from, so they are what to embed.
    let conn = rusqlite::Connection::open_with_flags(
        DB,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare("SELECT title, content FROM chunks")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;

    let mut titles: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for row in rows {
        let (title, content) = row?;
        // A handful of chunks are a bare `---` or empty. They carry no
        // topic, yet they scored top-5 for several Russian queries — keeping
        // them would measure the chunker, not the embedder.
        if content.trim().len() < 20 {
            continue;
        }
        titles.push(title);
        texts.push(content);
    }
    println!("chunks with content: {}", texts.len());

    // Which script each chunk is written in, so a cross-lingual hit (Russian
    // query, Latin-only chunk) is distinguishable from a same-script one.
    let script: Vec<Script> = texts.iter().map(|t| classify(t)).collect();
    let n = |s: Script| script.iter().filter(|x| **x == s).count();
    println!(
        "  latin-only {}  mixed {}  mostly-cyrillic {}\n",
        n(Script::Latin),
        n(Script::Mixed),
        n(Script::Cyrillic)
    );

    // The scorer's constructor is what resolves and commits the dynamic ONNX
    // Runtime dylib; a bare `TextEmbedding::try_new` deadlocks instead of
    // erroring if that has not happened. Warm it, then drop it — the model
    // below wants the raw vectors, which the scorer does not expose.
    drop(headroom_core::relevance::EmbeddingScorer::try_new_with_model(
        EmbeddingModel::MultilingualE5Small,
    )?);

    let (mut model, doc_prefix, query_prefix) = match which.as_str() {
        "user2" => {
            let dir = std::env::var("USER2_DIR").unwrap_or_else(|_| "/tmp/user2-onnx".into());
            let read = |f: &str| std::fs::read(format!("{dir}/{f}"));
            let files = fastembed::TokenizerFiles {
                tokenizer_file: read("tokenizer.json")?,
                config_file: read("config.json")?,
                special_tokens_map_file: read("special_tokens_map.json")?,
                tokenizer_config_file: read("tokenizer_config.json")?,
            };
            let mut udm =
                fastembed::UserDefinedEmbeddingModel::new(read("model.onnx")?, files);
            // fastembed defaults to CLS. USER2 mean-pools; taking the CLS
            // vector from a mean-pooled model reads a position the training
            // objective never constrained, which measures nothing.
            udm.pooling = Some(fastembed::Pooling::Mean);
            let opts = fastembed::InitOptionsUserDefined::new().with_max_length(512);
            (
                TextEmbedding::try_new_from_user_defined(udm, opts)?,
                "search_document: ",
                "search_query: ",
            )
        }
        _ => {
            let kind = match which.as_str() {
                "base" => EmbeddingModel::MultilingualE5Base,
                "large" => EmbeddingModel::MultilingualE5Large,
                _ => EmbeddingModel::MultilingualE5Small,
            };
            (
                TextEmbedding::try_new(InitOptions::new(kind))?,
                "passage: ",
                "query: ",
            )
        }
    };

    // Both families are trained with prefixes and lose a lot of retrieval
    // quality without them — `passage:`/`query:` for E5, `search_document:`/
    // `search_query:` for USER2.
    let t0 = std::time::Instant::now();
    let docs: Vec<String> = texts.iter().map(|t| format!("{doc_prefix}{t}")).collect();
    let doc_emb = model.embed(docs, Some(64))?;
    println!(
        "embedded {} chunks in {:.1}s\n",
        doc_emb.len(),
        t0.elapsed().as_secs_f64()
    );

    let mut all: Vec<String> = pairs
        .iter()
        .flat_map(|(en, ru)| [en.to_string(), ru.to_string()])
        .collect();
    // Full sentences, the shape a question actually arrives in.
    let sentences = [
        "где лежит база данных с памятью",
        "почему прокси падает с ошибкой соединения",
        "как настроить сжатие контекста",
        "which database holds the memory store",
    ];
    all.extend(sentences.iter().map(|s| s.to_string()));

    let tq = std::time::Instant::now();
    let qtexts: Vec<String> = all.iter().map(|q| format!("{query_prefix}{q}")).collect();
    let q_emb = model.embed(qtexts, None)?;
    println!(
        "embedded {} queries in {:.0}ms ({:.0}ms each)\n",
        q_emb.len(),
        tq.elapsed().as_secs_f64() * 1000.0,
        tq.elapsed().as_secs_f64() * 1000.0 / q_emb.len() as f64
    );

    println!(
        "{:<14} {:<16} {:>10} {:>9} {:>9}",
        "topic", "ru term", "overlap@10", "ru latin", "ru top"
    );
    println!("{}", "-".repeat(62));

    let mut detail = String::new();
    for (i, (en, ru)) in pairs.iter().enumerate() {
        let en_top = top_k(&q_emb[i * 2], &doc_emb, TOP_K);
        let ru_top = top_k(&q_emb[i * 2 + 1], &doc_emb, TOP_K);
        let overlap = ru_top
            .iter()
            .filter(|(d, _)| en_top.iter().any(|(e, _)| e == d))
            .count();
        let ru_latin = ru_top.iter().filter(|(d, _)| script[*d] == Script::Latin).count();
        let ru_best = ru_top.first().map(|(_, s)| *s).unwrap_or(0.0);
        println!("{en:<14} {ru:<16} {overlap:>10} {ru_latin:>9} {ru_best:>9.3}");

        detail.push_str(&format!("\n=== {en} | {ru} ===\n"));
        for (label, hits) in [("EN", &en_top), ("RU", &ru_top)] {
            for (d, s) in hits.iter().take(SHOW) {
                detail.push_str(&format!(
                    "  {label} {s:.3} [{:?}] {} :: {}\n",
                    script[*d],
                    trim(&titles[*d], 34),
                    trim(&texts[*d], 76)
                ));
            }
        }
    }
    println!("{detail}");

    println!("=== full-sentence queries ===");
    for (j, q) in sentences.iter().enumerate() {
        let hits = top_k(&q_emb[pairs.len() * 2 + j], &doc_emb, SHOW);
        println!("\n{q}");
        for (d, s) in hits {
            println!(
                "  {s:.3} [{:?}] {} :: {}",
                script[d],
                trim(&titles[d], 34),
                trim(&texts[d], 76)
            );
        }
    }
    Ok(())
}

#[cfg(feature = "ml")]
#[derive(PartialEq, Debug, Clone, Copy)]
enum Script {
    Latin,
    Mixed,
    Cyrillic,
}

#[cfg(feature = "ml")]
fn classify(t: &str) -> Script {
    let cyr = t.chars().filter(|c| ('\u{0400}'..='\u{04FF}').contains(c)).count();
    let lat = t.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if cyr == 0 {
        Script::Latin
    } else if cyr > lat {
        Script::Cyrillic
    } else {
        Script::Mixed
    }
}

#[cfg(feature = "ml")]
fn top_k(q: &[f32], docs: &[Vec<f32>], k: usize) -> Vec<(usize, f32)> {
    let mut scored: Vec<(usize, f32)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| (i, cosine(q, d)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(k);
    scored
}

#[cfg(feature = "ml")]
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

#[cfg(feature = "ml")]
fn trim(t: &str, max: usize) -> String {
    let one_line: String = t.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let mut end = max.min(one_line.len());
    while end > 0 && !one_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:<w$}", &one_line[..end], w = max)
}
