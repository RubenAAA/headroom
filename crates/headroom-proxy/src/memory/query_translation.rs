//! Rendering a Russian memory query in English.
//!
//! Measured on the live store: a Russian question reaches only Russian
//! memories. `прокси` returned 3 hits against `proxy`'s 225, and those 3 were
//! Russian-language memories rather than cross-lingual matches — so a question
//! asked in Russian could not see 93% of what was stored. Porter is an English
//! stemmer and a no-op on Cyrillic; trigram matches substrings, so a base form
//! finds an inflected one but never the reverse.
//!
//! Embedders were measured too. `MultilingualE5Small` and `Base` retrieved by
//! script before topic and returned Russian chunks on unrelated subjects;
//! `deepvk/USER2-small` fixed that but still lost to this map on every
//! term-shaped query, and cost a model download, a corpus backfill and
//! inference on the request path. This costs a table lookup.
//!
//! The translation *augments* the query rather than replacing it — the caller
//! searches for both — so the Russian memories that already matched keep
//! matching.

/// Russian stem to its English equivalent.
///
/// Stems, not words: Russian inflects by suffix, so `запрос` has to cover
/// `запроса`, `запросов` and `запросы`. Matching takes the longest stem that
/// prefixes the token, which is what keeps `формат` from being read as `форма`.
///
/// Drawn from the terms measured against the store plus the Cyrillic words
/// that actually occur in it. Add a line when a search comes back empty.
const STEMS: &[(&str, &str)] = &[
    // Measured pairs.
    ("прокси", "proxy"),
    ("кэш", "cache"),
    ("кеш", "cache"),
    ("сжати", "compression"),
    ("памят", "memory"),
    ("запрос", "query"),
    ("когорт", "cohort"),
    // Asking why something broke.
    ("ошибк", "error"),
    ("ошибок", "error"),
    ("соединени", "connection"),
    ("падает", "crash"),
    ("падал", "crash"),
    ("сбой", "failure"),
    // The shape of the data.
    ("таблиц", "table"),
    ("колонк", "column"),
    ("колонок", "column"),
    ("строк", "row"),
    ("файл", "file"),
    ("модел", "model"),
    ("страниц", "page"),
    ("отчёт", "report"),
    ("отчет", "report"),
    ("доход", "revenue"),
    ("бренд", "brand"),
    ("прогон", "run"),
    ("формат", "format"),
    ("форм", "form"),
    // The tools around it.
    ("ветк", "branch"),
    ("коммит", "commit"),
    ("тест", "test"),
    ("сервер", "server"),
    ("сервис", "service"),
    ("клиент", "client"),
    ("задач", "task"),
    ("ключ", "key"),
    ("сесси", "session"),
    ("верси", "version"),
    ("скрипт", "script"),
    ("индекс", "index"),
    ("поиск", "search"),
    ("логик", "logic"),
    ("лог", "log"),
    ("миграци", "migration"),
    ("конфиг", "config"),
    ("очеред", "queue"),
    ("пользовател", "user"),
];

/// Multi-word terms, matched before the table above splits the query up.
const PHRASES: &[(&str, &str)] = &[("база данных", "database"), ("базе данных", "database")];

/// `query` rendered in English, or `None` when nothing Russian was recognized.
///
/// Latin tokens survive untouched, so an identifier like `PROJ-1642` still
/// reaches the index. Cyrillic that maps to nothing is dropped rather than
/// carried over: it would only match the Russian chunks the untranslated query
/// already covers, and here it would be noise.
pub fn to_english(query: &str) -> Option<String> {
    let mut text = query.to_lowercase();
    let mut translated_any = false;
    for (ru, en) in PHRASES {
        if text.contains(ru) {
            text = text.replace(ru, en);
            translated_any = true;
        }
    }

    let mut out: Vec<String> = Vec::new();
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
        if token.is_empty() {
            continue;
        }
        if !is_cyrillic(token) {
            out.push(token.to_string());
            continue;
        }
        match longest_stem(token) {
            Some(en) => {
                translated_any = true;
                out.push(en.to_string());
            }
            // Unrecognized Russian: dropped, see the doc comment.
            None => continue,
        }
    }

    if !translated_any || out.is_empty() {
        return None;
    }
    Some(out.join(" "))
}

fn is_cyrillic(token: &str) -> bool {
    token
        .chars()
        .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

/// The English term for the longest stem that prefixes `token`.
///
/// Longest wins so that a stem which is itself the prefix of another — `форм`
/// under `формат`, `лог` under `логик` — does not shadow it.
fn longest_stem(token: &str) -> Option<&'static str> {
    STEMS
        .iter()
        .filter(|(ru, _)| token.starts_with(ru))
        .max_by_key(|(ru, _)| ru.len())
        .map(|(_, en)| *en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_term_translates() {
        assert_eq!(to_english("прокси").as_deref(), Some("proxy"));
    }

    /// The failing case from the measurement: this returned nothing useful
    /// through either the lexical path or an embedder.
    #[test]
    fn a_question_translates_word_by_word() {
        let out = to_english("почему прокси падает с ошибкой соединения").unwrap();
        assert!(out.contains("proxy"), "{out}");
        assert!(out.contains("crash"), "{out}");
        assert!(out.contains("error"), "{out}");
        assert!(out.contains("connection"), "{out}");
        // `почему` and `с` map to nothing and are dropped; carrying them over
        // was what made BM25 return chunks that merely contained "why".
        assert!(!out.contains("почему"), "{out}");
    }

    /// Russian inflects by suffix, so the stem has to cover the case endings.
    #[test]
    fn inflected_forms_reach_the_same_term() {
        for form in ["запрос", "запроса", "запросов", "запросы"] {
            assert_eq!(to_english(form).as_deref(), Some("query"), "{form}");
        }
    }

    #[test]
    fn a_two_word_term_translates_before_the_split() {
        assert_eq!(to_english("база данных").as_deref(), Some("database"));
    }

    /// The reason for longest-stem-first: `форм` prefixes `формат`.
    #[test]
    fn a_longer_stem_wins_over_a_shorter_one() {
        assert_eq!(to_english("формат").as_deref(), Some("format"));
        assert_eq!(to_english("форма").as_deref(), Some("form"));
        assert_eq!(to_english("логика").as_deref(), Some("logic"));
        assert_eq!(to_english("логи").as_deref(), Some("log"));
    }

    /// Identifiers are the thing lexical search is best at, and translating
    /// must not cost them.
    #[test]
    fn latin_tokens_survive() {
        assert_eq!(
            to_english("ошибка в PROJ-1642").as_deref(),
            Some("error proj-1642")
        );
    }

    #[test]
    fn an_english_query_is_left_alone() {
        assert_eq!(to_english("why does the proxy crash"), None);
    }

    /// Russian that maps to nothing is not a translation, and searching for an
    /// empty string would return everything.
    #[test]
    fn unrecognized_russian_yields_nothing() {
        assert_eq!(to_english("здравствуйте пожалуйста"), None);
    }
}
