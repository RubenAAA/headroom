//! Elide dense machine-generated lines (minified JS/CSS, base64, RSC payloads).
//!
//! Tool outputs that dump a fetched web page, a bundled asset, or an encoded
//! blob arrive as a few very long lines with almost no whitespace. None of the
//! structural compressors can do anything with them: they are not code an AST
//! walker can prune, not a log, not a table, and the HTML extractor returns
//! the whole thing (or nothing) once the page is mostly `<script>`. The result
//! is a multi-thousand-token block that rides through the router at ratio 1.0.
//!
//! This keeps a head and a tail of each such line and replaces the middle
//! with a one-line marker. Deliberately dumb: a line is "dense" when it is
//! long AND has a tiny fraction of spaces. Prose, code, logs, JSON with any
//! indentation, CSV, and markdown all carry far more than 6% spaces, so they
//! are never touched. A JSON-shaped line is left to SmartCrusher, and a lone
//! dense value (JWT, signed URL, PATH) is below the per-block minimum.

/// A line is dense only at or above this many chars.
pub const MIN_LINE_CHARS: usize = 300;
/// …and only when spaces are below this fraction of its length.
pub const MAX_SPACE_RATIO: f64 = 0.06;
/// A single dense line (a JWT, a signed URL) is a value the agent asked for,
/// not a dump: a block is only elided when its dense lines add up to at least
/// this many chars. Real bundle dumps are tens of KB.
pub const MIN_DENSE_TOTAL_CHARS: usize = 2000;
/// Kept head chars per elided line.
pub const HEAD_CHARS: usize = 160;
/// Kept tail chars per elided line.
pub const TAIL_CHARS: usize = 80;

/// True when `line` is long, nearly whitespace-free, and not JSON-shaped.
///
/// A compact JSON value (or a run of them) on one line is also nearly
/// whitespace-free, but that is SmartCrusher's job; the elider must never
/// pre-empt it.
pub fn is_dense_line(line: &str) -> bool {
    let n = line.chars().count();
    // Tabs: TSV / `psql -A` rows pass the space ratio but are data the agent
    // asked for; minified assets and encoded blobs never carry tabs.
    if n < MIN_LINE_CHARS || line.contains('\t') {
        return false;
    }
    let spaces = line.chars().filter(|&c| c == ' ').count();
    if spaces as f64 / n as f64 >= MAX_SPACE_RATIO {
        return false;
    }
    let stripped = line.trim();
    let (first, last) = (stripped.chars().next(), stripped.chars().last());
    !matches!(
        (first, last),
        (Some('{'), Some('}')) | (Some('['), Some(']'))
    )
}

/// Return `(text_with_dense_lines_elided, lines_elided)`.
///
/// Byte-identical to the input (and `0`) when no line is dense, so callers
/// can use inequality as the "did anything" signal. Line endings are
/// preserved: the split is on `'\n'` only, so `'\r'` stays on its line.
pub fn elide_dense_lines(text: &str) -> (String, usize) {
    if text.chars().count() < MIN_LINE_CHARS {
        return (text.to_string(), 0);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines
        .iter()
        .filter(|l| is_dense_line(l))
        .map(|l| l.chars().count())
        .sum::<usize>()
        < MIN_DENSE_TOTAL_CHARS
    {
        return (text.to_string(), 0);
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut n_elided = 0;
    for line in lines {
        if is_dense_line(line) {
            let chars: Vec<char> = line.chars().collect();
            let omitted = chars.len() - HEAD_CHARS - TAIL_CHARS;
            let head: String = chars[..HEAD_CHARS].iter().collect();
            let tail: String = chars[chars.len() - TAIL_CHARS..].iter().collect();
            out.push(format!(
                "{head} ...[{omitted} chars of dense machine-generated content elided]... {tail}"
            ));
            n_elided += 1;
        } else {
            out.push(line.to_string());
        }
    }
    if n_elided == 0 {
        return (text.to_string(), 0);
    }
    (out.join("\n"), n_elided)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dense_line(len: usize) -> String {
        "x".repeat(len)
    }

    #[test]
    fn short_text_passes_through() {
        let (out, n) = elide_dense_lines("hello");
        assert_eq!(out, "hello");
        assert_eq!(n, 0);
    }

    #[test]
    fn prose_and_code_are_untouched() {
        let prose = "the quick brown fox jumps over the lazy dog ".repeat(20);
        let (out, n) = elide_dense_lines(&prose);
        assert_eq!(out, prose);
        assert_eq!(n, 0);
    }

    #[test]
    fn json_shaped_lines_belong_to_smart_crusher() {
        let line = format!("{}{}{}", "{", "a".repeat(400), "}");
        assert!(!is_dense_line(&line));
        let arr = format!("[{}]", "1,".repeat(200));
        assert!(!is_dense_line(&arr));
    }

    #[test]
    fn tabbed_rows_are_data_not_dumps() {
        let line = format!("{}\t{}", "a".repeat(200), "b".repeat(200));
        assert!(!is_dense_line(&line));
    }

    #[test]
    fn lone_dense_value_is_below_minimum() {
        // One 500-char dense line: dense by itself, but the block total
        // (500) is under MIN_DENSE_TOTAL_CHARS.
        let (out, n) = elide_dense_lines(&dense_line(500));
        assert_eq!(n, 0);
        assert_eq!(out, dense_line(500));
    }

    #[test]
    fn bundle_dump_elides_middle_keeps_head_tail() {
        let line_a = "a".repeat(3000);
        let line_b = "b".repeat(3000);
        let text = format!("header line\n{line_a}\nfooter line\n{line_b}");
        let (out, n) = elide_dense_lines(&text);
        assert_eq!(n, 2);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "header line");
        assert_eq!(lines[2], "footer line");
        assert!(lines[1].starts_with(&"a".repeat(HEAD_CHARS)));
        assert!(lines[1].ends_with(&"a".repeat(TAIL_CHARS)));
        assert!(lines[1].contains("2760 chars of dense machine-generated content elided"));
    }

    #[test]
    fn crlf_endings_stay_on_their_line() {
        let line = "z".repeat(2500);
        let text = format!("{line}\r\nother\r\n{}", "y".repeat(2500));
        let (out, n) = elide_dense_lines(&text);
        assert_eq!(n, 2);
        assert!(out.contains('\r'));
    }
}
