//! HTMLExtractor — Rust port of `headroom.transforms.html_extractor`.
//!
//! Extracts main content from HTML pages, removing structural noise like
//! scripts, styles, navigation, ads, and footers. This is content
//! extraction, not compression — we remove irrelevant blocks, not tokens.
//! Typical reduction: 70-90% with zero content loss.
//!
//! # Parity scope (functional, NOT byte parity)
//!
//! The Python reference uses `trafilatura`, which has no Rust equivalent.
//! This port uses `dom_smoothie` (a Rust port of Mozilla Readability.js)
//! as a functional replacement: main content survives, boilerplate is
//! stripped, but the extracted text is NOT byte-identical to trafilatura's
//! output — different heuristics, different formatting. What IS exact:
//! the API shapes, the empty/whitespace and failure fallbacks, the
//! ratio/length math (char counts, matching Python `len()` semantics),
//! and `is_html_content` (stdlib-only in Python, ported exactly).
//!
//! # Config fields that are no-ops in Rust
//!
//! `dom_smoothie` has no equivalents for several trafilatura knobs; these
//! stay in [`HtmlExtractorConfig`] for shape parity but do not affect
//! extraction: `include_links`, `include_images`, `include_tables`,
//! `include_comments`, `favor_precision`, `favor_recall`. What IS honored:
//! `output_format` (maps to `TextMode::Markdown` / `TextMode::Formatted`),
//! `include_formatting` (false forces `TextMode::Raw`), `extract_metadata`.
//!
//! # Metadata divergences
//!
//! `title`/`author`/`date` map from Readability's `title`/`byline`/
//! `published_time`. The metadata map carries the same keys Python emits
//! (`title`, `author`, `date`, `sitename`, `description`, `categories`,
//! `tags`), but `categories`/`tags` are always `null` — Readability.js has
//! no category/tag extraction.

use std::collections::BTreeMap;

use dom_smoothie::{Config as SmoothieConfig, Readability, TextMode};
use serde_json::Value;

// ─── Result ─────────────────────────────────────────────────────────────

/// Result of HTML content extraction. Mirrors Python `HTMLExtractionResult`.
#[derive(Debug, Clone)]
pub struct HtmlExtractionResult {
    pub extracted: String,
    pub original: String,
    /// Length in CHARS (Python `len(str)` semantics), not bytes.
    pub original_length: usize,
    /// Length in CHARS (Python `len(str)` semantics), not bytes.
    pub extracted_length: usize,
    pub compression_ratio: f64,
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    /// Extra metadata (title/author/date/sitename/description/categories/
    /// tags). BTreeMap for deterministic iteration; empty when metadata
    /// extraction is disabled or nothing was found.
    pub metadata: BTreeMap<String, Value>,
}

impl HtmlExtractionResult {
    /// Percentage of content removed. 0.0 when the original was empty.
    pub fn reduction_percent(&self) -> f64 {
        if self.original_length == 0 {
            return 0.0;
        }
        (1.0 - self.compression_ratio) * 100.0
    }
}

// ─── Config ─────────────────────────────────────────────────────────────

/// Configuration for HTML extraction. Mirrors Python `HTMLExtractorConfig`.
///
/// See the module docs for which fields are no-ops under `dom_smoothie`.
#[derive(Debug, Clone)]
pub struct HtmlExtractorConfig {
    /// "markdown" or "txt"/"text". Anything else falls back to markdown.
    pub output_format: String,
    /// No-op in Rust (trafilatura-only knob).
    pub include_links: bool,
    /// No-op in Rust (trafilatura-only knob).
    pub include_images: bool,
    /// No-op in Rust (trafilatura-only knob).
    pub include_tables: bool,
    /// No-op in Rust (trafilatura-only knob).
    pub include_comments: bool,
    /// `false` forces `TextMode::Raw` (unformatted text) regardless of
    /// `output_format`.
    pub include_formatting: bool,
    /// No-op in Rust (trafilatura-only knob).
    pub favor_precision: bool,
    /// No-op in Rust (trafilatura-only knob).
    pub favor_recall: bool,
    pub extract_metadata: bool,
}

impl Default for HtmlExtractorConfig {
    fn default() -> Self {
        Self {
            output_format: "markdown".to_string(),
            include_links: true,
            include_images: false,
            include_tables: true,
            include_comments: false,
            include_formatting: true,
            favor_precision: false,
            favor_recall: true,
            extract_metadata: true,
        }
    }
}

// ─── Extractor ──────────────────────────────────────────────────────────

/// Extracts main content from HTML pages. Mirrors Python `HTMLExtractor`.
///
/// Uses `dom_smoothie` (Readability.js port) for content extraction. This
/// is not compression — it's removing structural HTML noise (scripts,
/// styles, nav, ads) to get the actual content the user wanted.
#[derive(Debug, Clone, Default)]
pub struct HtmlExtractor {
    pub config: HtmlExtractorConfig,
}

impl HtmlExtractor {
    pub fn new(config: HtmlExtractorConfig) -> Self {
        Self { config }
    }

    fn smoothie_config(&self) -> SmoothieConfig {
        let text_mode = if !self.config.include_formatting {
            TextMode::Raw
        } else {
            match self.config.output_format.as_str() {
                "txt" | "text" => TextMode::Formatted,
                _ => TextMode::Markdown,
            }
        };
        SmoothieConfig {
            text_mode,
            ..SmoothieConfig::default()
        }
    }

    /// Extract main content from HTML.
    ///
    /// - Empty/whitespace-only input → zeroed result (ratio 0.0).
    /// - Extraction failure (garbage HTML, no readable content) →
    ///   `extracted = ""`, same as Python's `None → ""` fallback.
    /// - `url` helps resolve relative links; a non-absolute URL is ignored
    ///   (trafilatura tolerates any URL, `dom_smoothie` rejects relative
    ///   ones — we retry without it rather than fail).
    pub fn extract(&self, html: &str, url: Option<&str>) -> HtmlExtractionResult {
        let original_length = html.chars().count();

        if html.trim().is_empty() {
            return HtmlExtractionResult {
                extracted: String::new(),
                original: html.to_string(),
                original_length,
                extracted_length: 0,
                compression_ratio: 0.0,
                title: None,
                author: None,
                date: None,
                metadata: BTreeMap::new(),
            };
        }

        let cfg = self.smoothie_config();
        // `Readability::new` only errors on a non-absolute `document_url`;
        // retry without the URL so a bad URL never fails extraction.
        let readability = Readability::new(html, url, Some(cfg.clone()))
            .or_else(|_| Readability::new(html, None, Some(cfg)));

        let mut extracted = String::new();
        let mut title = None;
        let mut author = None;
        let mut date = None;
        let mut metadata = BTreeMap::new();

        if let Ok(mut readability) = readability {
            // Metadata comes from <meta>/JSON-LD tags, independent of
            // whether content extraction succeeds (mirrors Python's
            // separate `trafilatura.extract_metadata` call). Grab it
            // BEFORE `parse()` mutates the DOM.
            if self.config.extract_metadata {
                let json_ld = readability.parse_json_ld();
                let meta = readability.get_article_metadata(json_ld);
                title = Some(meta.title).filter(|t| !t.is_empty());
                author = meta.byline.clone();
                date = meta.published_time.clone();
                let has_any = title.is_some()
                    || meta.byline.is_some()
                    || meta.published_time.is_some()
                    || meta.site_name.is_some()
                    || meta.excerpt.is_some();
                if has_any {
                    let opt = |v: Option<String>| v.map(Value::String).unwrap_or(Value::Null);
                    metadata.insert("title".into(), opt(title.clone()));
                    metadata.insert("author".into(), opt(meta.byline));
                    metadata.insert("date".into(), opt(meta.published_time));
                    metadata.insert("sitename".into(), opt(meta.site_name));
                    metadata.insert("description".into(), opt(meta.excerpt));
                    // Readability has no category/tag extraction; Python's
                    // trafilatura fields are carried as null for key parity.
                    metadata.insert("categories".into(), Value::Null);
                    metadata.insert("tags".into(), Value::Null);
                }
            }

            // Extraction failure → empty string, exactly like Python's
            // `if extracted is None: extracted = ""`.
            if let Ok(article) = readability.parse() {
                extracted = article.text_content.to_string();
            }
        }

        let extracted_length = extracted.chars().count();
        let compression_ratio = extracted_length as f64 / original_length.max(1) as f64;

        HtmlExtractionResult {
            extracted,
            original: html.to_string(),
            original_length,
            extracted_length,
            compression_ratio,
            title,
            author,
            date,
            metadata,
        }
    }

    /// Extract content from multiple HTML pages, in input order.
    pub fn extract_batch(
        &self,
        html_contents: &[(&str, Option<&str>)],
    ) -> Vec<HtmlExtractionResult> {
        html_contents
            .iter()
            .map(|(html, url)| self.extract(html, *url))
            .collect()
    }
}

// ─── Detection ──────────────────────────────────────────────────────────

/// Check if content appears to be HTML. Exact port of Python
/// `is_html_content` (stdlib-only, byte-for-byte decision parity).
pub fn is_html_content(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }

    let stripped = content.trim().to_lowercase();

    // Check for DOCTYPE or html tag
    if stripped.starts_with("<!doctype html") || stripped.starts_with("<html") {
        return true;
    }

    // Check for common HTML patterns within the first 2000 CHARS
    // (Python slices the str: `stripped[:2000]`).
    let window: String = stripped.chars().take(2000).collect();
    let html_indicators = [
        "<head",
        "<body",
        "<div",
        "<script",
        "<style",
        "<meta",
        "<link",
        "<!doctype",
    ];
    let matches = html_indicators
        .iter()
        .filter(|indicator| window.contains(**indicator))
        .count();

    // If we see multiple HTML-specific tags, it's likely HTML
    matches >= 2
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_html_content (exact-parity matrix from Python TestIsHtmlContent) ──

    #[test]
    fn detects_doctype_html() {
        assert!(is_html_content(
            "<!DOCTYPE html><html><body>Content</body></html>"
        ));
    }

    #[test]
    fn detects_html_tag() {
        assert!(is_html_content(
            "<html><head></head><body>Content</body></html>"
        ));
    }

    #[test]
    fn detects_structural_tags() {
        assert!(is_html_content(
            "<html><div><nav>Menu</nav><article>Content</article><footer>Footer</footer></div></html>"
        ));
    }

    #[test]
    fn detects_two_indicators_without_html_tag() {
        // 2+ indicators within the first 2000 chars, no <html>/<!doctype>
        assert!(is_html_content("<div><script>x()</script></div>"));
    }

    #[test]
    fn rejects_single_indicator() {
        assert!(!is_html_content("<div>only one indicator</div>"));
    }

    #[test]
    fn rejects_indicators_beyond_2000_chars() {
        // Indicators exist but past the 2000-char window → not counted
        let content = format!("{}{}", "x".repeat(2001), "<div><script>");
        assert!(!is_html_content(&content));
    }

    #[test]
    fn rejects_plain_text() {
        assert!(!is_html_content("This is just plain text with no HTML."));
    }

    #[test]
    fn rejects_json() {
        assert!(!is_html_content(r#"{"name": "test", "value": 123}"#));
    }

    #[test]
    fn rejects_markdown() {
        assert!(!is_html_content(
            "# Heading\n\nParagraph with **bold** text."
        ));
    }

    #[test]
    fn rejects_code() {
        assert!(!is_html_content("def hello():\n    print('world')"));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_html_content(""));
    }

    #[test]
    fn case_insensitive() {
        assert!(is_html_content(
            "<!DOCTYPE HTML><HTML><BODY>Content</BODY></HTML>"
        ));
    }

    // ── Extractor behavior (library-agnostic assertions) ──

    fn extractor() -> HtmlExtractor {
        HtmlExtractor::default()
    }

    const ARTICLE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Breaking News: Important Event Happens</title>
    <script>window.analytics = {};</script>
    <style>.ad { display: block; }</style>
</head>
<body>
    <header>
        <nav class="main-nav">
            <a href="/">Home</a>
            <a href="/news">News</a>
            <a href="/sports">Sports</a>
        </nav>
    </header>
    <div class="ad-banner">Advertisement Here</div>
    <main>
        <article class="news-article">
            <h1>Breaking News: Important Event Happens</h1>
            <p class="byline">By Jane Reporter | January 15, 2024</p>
            <p>In a surprising turn of events, something important happened today
            that will affect millions of people around the world.</p>
            <p>Experts say this development represents a major shift in how
            we think about the topic at hand.</p>
            <p>"This is truly unprecedented," said Dr. Expert, a leading
            authority in the field.</p>
            <p>The implications of this event are still being analyzed, but
            early reports suggest significant changes ahead.</p>
        </article>
    </main>
    <aside class="sidebar">
        <h3>Trending Stories</h3>
        <ul>
            <li><a href="/story1">Story 1</a></li>
            <li><a href="/story2">Story 2</a></li>
        </ul>
    </aside>
    <footer>
        <p>&copy; 2024 News Site</p>
        <a href="/privacy">Privacy Policy</a>
    </footer>
    <script>trackPageView();</script>
</body>
</html>"#;

    #[test]
    fn extracts_article_and_strips_boilerplate() {
        let result = extractor().extract(ARTICLE_HTML, None);

        // Main article content survives. (Divergence from trafilatura:
        // Readability drops the <h1> duplicating the <title> and moves the
        // headline into `title`; markdown mode escapes punctuation, so we
        // assert on punctuation-free body substrings.)
        assert!(result.extracted.contains("surprising turn of events"));
        assert!(result.extracted.contains("major shift"));
        assert!(result.extracted.contains("significant changes ahead"));
        assert_eq!(
            result.title.as_deref(),
            Some("Breaking News: Important Event Happens")
        );

        // Script/style noise is removed
        assert!(!result.extracted.contains("trackPageView"));
        assert!(!result.extracted.contains("window.analytics"));
        assert!(!result.extracted.contains("display: block"));

        // Nav/footer boilerplate is stripped (mirrors Python's OR-assertion:
        // at least one of nav/footer must be gone)
        assert!(
            !result.extracted.contains("Privacy Policy") || !result.extracted.contains("main-nav")
        );

        // Significant reduction
        assert!(result.compression_ratio < 0.5);
        assert!(result.reduction_percent() > 50.0);
    }

    #[test]
    fn extracts_metadata() {
        let html = r#"<!DOCTYPE html>
<html>
<head>
    <title>Page Title for Testing</title>
    <meta name="author" content="John Doe">
    <meta name="description" content="A test page description">
</head>
<body>
    <article>
        <h1>Page Title for Testing</h1>
        <p>Content here. Enough words so the extractor has something real
        to hold onto when scoring candidate nodes for readability.</p>
    </article>
</body>
</html>"#;
        let result = extractor().extract(html, None);

        assert_eq!(result.title.as_deref(), Some("Page Title for Testing"));
        assert_eq!(
            result.metadata.get("title"),
            Some(&Value::String("Page Title for Testing".to_string()))
        );
        // trafilatura-only fields carried as null for key parity
        assert_eq!(result.metadata.get("categories"), Some(&Value::Null));
    }

    #[test]
    fn disable_metadata_extraction() {
        let config = HtmlExtractorConfig {
            extract_metadata: false,
            ..Default::default()
        };
        let result = HtmlExtractor::new(config).extract(
            "<!DOCTYPE html><html><head><title>Test Title</title></head><body><article><p>Content.</p></article></body></html>",
            None,
        );

        assert!(result.title.is_none());
        assert!(result.metadata.is_empty());
    }

    #[test]
    fn handles_empty_html() {
        let result = extractor().extract("", None);

        assert_eq!(result.extracted, "");
        assert_eq!(result.original_length, 0);
        assert_eq!(result.extracted_length, 0);
        assert_eq!(result.compression_ratio, 0.0);
        assert_eq!(result.reduction_percent(), 0.0);
    }

    #[test]
    fn handles_whitespace_only() {
        let result = extractor().extract("   \n\t  ", None);

        assert_eq!(result.extracted, "");
        assert_eq!(result.compression_ratio, 0.0);
        // Whitespace still counts toward original_length (Python len())
        assert_eq!(result.original_length, 7);
    }

    #[test]
    fn garbage_html_does_not_panic() {
        // Malformed tags, truncated entities, binary-ish noise
        for garbage in [
            "<<<>>><p<div<<html",
            "<html><body><div>&#xZZ;<span></body>",
            "\u{0}\u{1}\u{2}<html>\u{fffd}<body",
            "<p>Just a paragraph.</p>",
        ] {
            let result = extractor().extract(garbage, None);
            // Failure path → extracted = "" (never a panic); success is
            // also fine — we only require a String result.
            assert!(result.original_length > 0);
            let _ = result.extracted;
        }
    }

    #[test]
    fn bad_url_is_ignored_not_fatal() {
        let result = extractor().extract(ARTICLE_HTML, Some("not-an-absolute-url"));
        assert!(result.extracted.contains("surprising turn of events"));
    }

    #[test]
    fn extraction_failure_ratio_math() {
        // No readable content → extracted "" → ratio 0 with non-zero original
        let result = extractor().extract("<html><body></body></html>", None);
        assert_eq!(result.extracted, "");
        assert_eq!(result.extracted_length, 0);
        assert_eq!(result.compression_ratio, 0.0);
        assert!(result.original_length > 0);
    }

    #[test]
    fn ratio_and_lengths_are_char_counts() {
        // Multi-byte chars: lengths must match Python len() (chars, not bytes)
        let html = "<html><body>héllo wörld</body></html>";
        let result = extractor().extract(html, None);
        assert_eq!(result.original_length, html.chars().count());
        assert_eq!(result.extracted_length, result.extracted.chars().count());
    }

    #[test]
    fn reduction_percent_calculation() {
        let result = HtmlExtractionResult {
            extracted: "short".to_string(),
            original: "much longer content here".to_string(),
            original_length: 100,
            extracted_length: 25,
            compression_ratio: 0.25,
            title: None,
            author: None,
            date: None,
            metadata: BTreeMap::new(),
        };
        assert_eq!(result.reduction_percent(), 75.0);
    }

    #[test]
    fn reduction_percent_with_zero_original() {
        let result = HtmlExtractionResult {
            extracted: String::new(),
            original: String::new(),
            original_length: 0,
            extracted_length: 0,
            compression_ratio: 0.0,
            title: None,
            author: None,
            date: None,
            metadata: BTreeMap::new(),
        };
        assert_eq!(result.reduction_percent(), 0.0);
    }

    #[test]
    fn extract_batch_preserves_order_and_count() {
        let pages: Vec<(&str, Option<&str>)> = vec![
            (
                "<html><body><article><p>Page one content.</p></article></body></html>",
                Some("http://example.com/page1"),
            ),
            (
                "<html><body><article><p>Page two content.</p></article></body></html>",
                Some("http://example.com/page2"),
            ),
            (
                "<html><body><article><p>Page three content.</p></article></body></html>",
                None,
            ),
        ];

        let results = extractor().extract_batch(&pages);
        assert_eq!(results.len(), 3);
        for (result, (html, _)) in results.iter().zip(&pages) {
            assert_eq!(result.original, *html);
        }
    }
}
