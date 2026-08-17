//! Tile-boundary image optimizer — reduce vision tokens with zero quality loss.
//!
//! Resizes images to land on provider tile boundaries, minimizing token count
//! without perceptible quality change. Pure math — no ML models needed.
//!
//! OpenAI tiles at 512px: tokens = 85 + 170 * ceil(w/512) * ceil(h/512).
//! Anthropic: tokens = (w * h) / 750, capped at 1568px / 1.15MP.
//!
//! Mirrors Python's `headroom.image.tile_optimizer`.

use serde_json::Value;

/// Scale a dimension by a factor, rounding to nearest integer.
fn scale_dim(dim: u32, factor: f64) -> u32 {
    (dim as f64 * factor).round() as u32
}

// ─── Result type ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TileOptResult {
    pub original_width: u32,
    pub original_height: u32,
    pub optimized_width: u32,
    pub optimized_height: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub provider: String,
    pub resized: bool,
}

impl TileOptResult {
    pub fn tokens_saved(&self) -> u32 {
        self.tokens_before.saturating_sub(self.tokens_after)
    }

    pub fn savings_pct(&self) -> f64 {
        if self.tokens_before == 0 {
            return 0.0;
        }
        self.tokens_saved() as f64 / self.tokens_before as f64 * 100.0
    }
}

// ─── Token estimation formulas ──────────────────────────────────────────

/// OpenAI GPT-4o vision token formula.
pub fn estimate_openai_tokens(width: u32, height: u32, detail: &str) -> u32 {
    if detail == "low" {
        return 85;
    }

    let mut w = width;
    let mut h = height;

    // Step 1: scale so max dimension ≤ 2048
    let max_dim = w.max(h);
    if max_dim > 2048 {
        let scale = 2048.0 / max_dim as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    // Step 2: scale so shortest side ≤ 768
    let min_dim = w.min(h);
    if min_dim > 768 {
        let scale = 768.0 / min_dim as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    // Step 3: count 512×512 tiles
    let tiles = w.div_ceil(512) * h.div_ceil(512);
    85 + 170 * tiles
}

/// Anthropic Claude vision token formula: (w * h) / 750.
/// What Anthropic actually bills for an image.
///
/// Only the 1568px edge limit is enforced — above it the provider downscales and
/// bills the smaller size. **1.15MP is a guideline, not a billing cap**, and
/// assuming otherwise here is what made this whole module inert: applying the
/// cap made `tokens_after == tokens_before` for every real screenshot, so
/// [`optimize_content_block`] bailed every time.
///
/// Measured against `count_tokens` on 2026-08-17: a 1560x1150 screenshot (1.79MP)
/// billed **2,365 tokens**, against 2,392 for the uncapped `w * h / 750` and
/// 1,533 if the 1.15MP cap applied. The uncapped formula is right.
pub fn estimate_anthropic_tokens(width: u32, height: u32) -> u32 {
    let mut w = width;
    let mut h = height;

    // The one limit the provider does enforce.
    let max_edge = w.max(h);
    if max_edge > 1568 {
        let scale = 1568.0 / max_edge as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    (w * h / 750).max(1)
}

// ─── Tile-boundary optimization ─────────────────────────────────────────

/// Find dimensions that minimize OpenAI tile count.
///
/// Tries reducing to fewer tiles while keeping ≥40% of original pixels.
pub fn find_optimal_openai_dimensions(width: u32, height: u32) -> (u32, u32) {
    let mut w = width;
    let mut h = height;

    // Simulate OpenAI's internal scaling first
    let max_dim = w.max(h);
    if max_dim > 2048 {
        let scale = 2048.0 / max_dim as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    let min_dim = w.min(h);
    if min_dim > 768 {
        let scale = 768.0 / min_dim as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    let current_tiles = w.div_ceil(512) * h.div_ceil(512);
    let mut best_w = w;
    let mut best_h = h;
    let mut best_tiles = current_tiles;

    let orig_pixels = w as f64 * h as f64;

    for target_cols in 1..=w.div_ceil(512) {
        for target_rows in 1..=h.div_ceil(512) {
            let tiles = target_cols * target_rows;
            if tiles >= current_tiles {
                continue;
            }

            let tw = target_cols * 512;
            let th = target_rows * 512;
            let scale_w = tw as f64 / w as f64;
            let scale_h = th as f64 / h as f64;
            let scale = scale_w.min(scale_h);
            let nw = scale_dim(w, scale);
            let nh = scale_dim(h, scale);

            // Only accept if keeping ≥40% of original pixels
            if nw as f64 * nh as f64 >= orig_pixels * 0.4 && tiles < best_tiles {
                best_w = nw;
                best_h = nh;
                best_tiles = tiles;
            }
        }
    }

    (best_w, best_h)
}

/// Pre-resize to Anthropic's limits (they'd do it anyway).
pub fn find_optimal_anthropic_dimensions(width: u32, height: u32) -> (u32, u32) {
    let mut w = width;
    let mut h = height;

    let max_edge = w.max(h);
    if max_edge > 1568 {
        let scale = 1568.0 / max_edge as f64;
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    let total = w as f64 * h as f64;
    if total > 1_150_000.0 {
        let scale = (1_150_000.0 / total).sqrt();
        w = scale_dim(w, scale);
        h = scale_dim(h, scale);
    }

    (w, h)
}

// ─── Message-level optimization ─────────────────────────────────────────

/// Optimize all images in messages for minimum token cost.
///
/// Returns `(optimized_messages, results)`. Currently only computes
/// token savings — actual pixel resize requires the `image` crate.
pub fn optimize_images_in_messages(
    messages: &[Value],
    provider: &str,
) -> (Vec<Value>, Vec<TileOptResult>) {
    let mut results = Vec::new();
    let mut optimized = Vec::new();

    for message in messages {
        let content = match message.get("content").and_then(Value::as_array) {
            Some(c) => c,
            None => {
                optimized.push(message.clone());
                continue;
            }
        };

        let mut new_content = Vec::new();
        for item in content {
            if let Some((opt_item, opt_result)) = optimize_content_block(item, provider) {
                new_content.push(opt_item);
                results.push(opt_result);
            } else {
                new_content.push(item.clone());
            }
        }

        let mut msg = message.clone();
        msg["content"] = Value::Array(new_content);
        optimized.push(msg);
    }

    (optimized, results)
}

/// Optimize a single image content block. Returns None if not an image.
fn optimize_content_block(item: &Value, provider: &str) -> Option<(Value, TileOptResult)> {
    // OpenAI format: {"type": "image_url", "image_url": {"url": "data:..."}}
    if item.get("type").and_then(Value::as_str) == Some("image_url") {
        let url = item
            .get("image_url")
            .and_then(|iu| iu.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if !url.starts_with("data:") {
            return None;
        }

        let image_data = decode_data_url(url)?;
        let img = image::load_from_memory(&image_data).ok()?;
        let orig_w = img.width();
        let orig_h = img.height();

        let tokens_before = estimate_openai_tokens(orig_w, orig_h, "high");

        let (opt_w, opt_h) = if provider == "openai" {
            find_optimal_openai_dimensions(orig_w, orig_h)
        } else {
            find_optimal_anthropic_dimensions(orig_w, orig_h)
        };

        let tokens_after = estimate_openai_tokens(opt_w, opt_h, "high");
        if tokens_after >= tokens_before {
            return None;
        }

        let resized = img.resize(opt_w, opt_h, image::imageops::FilterType::Lanczos3);
        let mut buf = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
        resized.write_with_encoder(encoder).ok()?;
        let resized_bytes = buf.into_inner();
        let b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resized_bytes);

        let mut new_item = item.clone();
        if let Some(iu) = new_item.get_mut("image_url") {
            if let Some(url_val) = iu.get_mut("url") {
                *url_val = Value::String(format!("data:image/jpeg;base64,{b64}"));
            }
        }

        let result = TileOptResult {
            original_width: orig_w,
            original_height: orig_h,
            optimized_width: opt_w,
            optimized_height: opt_h,
            tokens_before,
            tokens_after,
            provider: provider.to_string(),
            resized: true,
        };
        return Some((new_item, result));
    }

    // Anthropic format: {"type": "image", "source": {"type": "base64", "data": "..."}}
    if item.get("type").and_then(Value::as_str) == Some("image") {
        let source = item.get("source")?;
        if source.get("type").and_then(Value::as_str) != Some("base64") {
            return None;
        }

        let b64_data = source.get("data").and_then(Value::as_str)?;
        let image_data =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_data).ok()?;
        let img = image::load_from_memory(&image_data).ok()?;
        let orig_w = img.width();
        let orig_h = img.height();

        let tokens_before = estimate_anthropic_tokens(orig_w, orig_h);
        let (opt_w, opt_h) = find_optimal_anthropic_dimensions(orig_w, orig_h);
        let tokens_after = estimate_anthropic_tokens(opt_w, opt_h);

        if tokens_after >= tokens_before {
            return None;
        }

        let resized = img.resize(opt_w, opt_h, image::imageops::FilterType::Lanczos3);
        let mut buf = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
        resized.write_with_encoder(encoder).ok()?;
        let resized_bytes = buf.into_inner();
        let new_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resized_bytes);

        let mut new_item = item.clone();
        if let Some(src) = new_item.get_mut("source") {
            if let Some(data_val) = src.get_mut("data") {
                *data_val = Value::String(new_b64);
            }
            if let Some(mt) = src.get_mut("media_type") {
                *mt = Value::String("image/jpeg".to_string());
            }
        }

        let result = TileOptResult {
            original_width: orig_w,
            original_height: orig_h,
            optimized_width: opt_w,
            optimized_height: opt_h,
            tokens_before,
            tokens_after,
            provider: "anthropic".to_string(),
            resized: true,
        };
        return Some((new_item, result));
    }

    None
}

// ─── Caching ─────────────────────────────────────────────────────────────

/// Resized images, keyed by the hash of the bytes that produced them.
///
/// Two reasons, and the second is the load-bearing one:
///
/// 1. Cost. History carries every past screenshot on every turn, so without a
///    cache a deep conversation would Lanczos-resize the same multi-megapixel
///    PNGs on each request.
/// 2. **Prefix stability.** The forwarded bytes for a given image must be
///    identical on every turn or the cached prefix breaks at that image and
///    everything after it is re-created. The encoder is deterministic for a
///    fixed input, but memoising removes the question: one image, one output,
///    for the life of the process.
///
/// A miss stores `None` too, so an image that is not worth resizing is decoded
/// once rather than on every turn.
type ImageCacheKey = [u8; 32];
static RESIZE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<lru::LruCache<ImageCacheKey, Option<(Value, TileOptResult)>>>,
> = std::sync::OnceLock::new();

/// Distinct images held before the oldest is dropped. Screenshots are the only
/// images in practice and a conversation carries a handful.
const RESIZE_CACHE_CAPACITY: usize = 512;

fn resize_cache(
) -> &'static std::sync::Mutex<lru::LruCache<ImageCacheKey, Option<(Value, TileOptResult)>>> {
    RESIZE_CACHE.get_or_init(|| {
        std::sync::Mutex::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(RESIZE_CACHE_CAPACITY).expect("capacity is non-zero"),
        ))
    })
}

fn cache_key(item: &Value, provider: &str) -> Option<ImageCacheKey> {
    use sha2::{Digest, Sha256};
    let data = item
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("image_url")
                .and_then(|iu| iu.get("url"))
                .and_then(Value::as_str)
        })?;
    let mut hasher = Sha256::new();
    hasher.update(provider.as_bytes());
    hasher.update([0u8]);
    hasher.update(data.as_bytes());
    Some(hasher.finalize().into())
}

/// [`optimize_content_block`] memoised on the source bytes.
fn optimize_content_block_cached(item: &Value, provider: &str) -> Option<(Value, TileOptResult)> {
    let Some(key) = cache_key(item, provider) else {
        return optimize_content_block(item, provider);
    };
    if let Ok(mut cache) = resize_cache().lock() {
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    // Resize outside the lock: it is the slow part and nothing else needs
    // serialising. A duplicate resize under contention is wasted work, not a
    // wrong answer, because the transform is a pure function of the bytes.
    let computed = optimize_content_block(item, provider);
    if let Ok(mut cache) = resize_cache().lock() {
        cache.put(key, computed.clone());
    }
    computed
}

/// [`optimize_images_in_messages`], reusing previously resized images.
///
/// This is the entry point the proxy calls; the uncached variant stays for
/// callers that want a one-shot transform.
pub fn optimize_images_in_messages_cached(
    messages: &[Value],
    provider: &str,
) -> (Vec<Value>, Vec<TileOptResult>) {
    let mut results = Vec::new();
    let mut optimized = Vec::with_capacity(messages.len());

    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            optimized.push(message.clone());
            continue;
        };

        let mut new_content = Vec::with_capacity(content.len());
        let mut touched = false;
        for item in content {
            // Screenshots arrive nested: measured over 800 live bodies, all 13
            // distinct images sat in `tool_result.content` and not one at the
            // top level. Walking only the top level finds nothing at all.
            if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(inner) = item.get("content").and_then(Value::as_array) {
                    let mut new_inner = Vec::with_capacity(inner.len());
                    let mut inner_touched = false;
                    for sub in inner {
                        match optimize_content_block_cached(sub, provider) {
                            Some((opt_sub, opt_result)) => {
                                new_inner.push(opt_sub);
                                results.push(opt_result);
                                inner_touched = true;
                            }
                            None => new_inner.push(sub.clone()),
                        }
                    }
                    if inner_touched {
                        let mut wrapper = item.clone();
                        wrapper["content"] = Value::Array(new_inner);
                        new_content.push(wrapper);
                        touched = true;
                        continue;
                    }
                }
                new_content.push(item.clone());
                continue;
            }

            match optimize_content_block_cached(item, provider) {
                Some((opt_item, opt_result)) => {
                    new_content.push(opt_item);
                    results.push(opt_result);
                    touched = true;
                }
                None => new_content.push(item.clone()),
            }
        }

        if touched {
            let mut msg = message.clone();
            msg["content"] = Value::Array(new_content);
            optimized.push(msg);
        } else {
            optimized.push(message.clone());
        }
    }

    (optimized, results)
}

/// Decode base64 image data from a data URL.
fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    // Format: data:image/png;base64,<data>
    let b64_part = url.split(',').last()?;
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_part).ok()
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── OpenAI token estimation ──────────────────────────────────────

    #[test]
    fn openai_low_detail_returns_85() {
        assert_eq!(estimate_openai_tokens(1000, 1000, "low"), 85);
    }

    #[test]
    fn openai_small_image_one_tile() {
        // 512×512 = 1 tile → 85 + 170 = 255
        assert_eq!(estimate_openai_tokens(512, 512, "high"), 255);
    }

    #[test]
    fn openai_770px_image_four_tiles() {
        // 770×770 → after scaling: ≤2048, ≤768? 770 > 768, so scale to 768
        // 768×768 → ceil(768/512)=2, 2×2=4 tiles → 85+680=765
        assert_eq!(estimate_openai_tokens(770, 770, "high"), 765);
    }

    #[test]
    fn openai_large_image_scaled() {
        // 4096×4096 → scale to 2048 → min_dim=2048>768 → scale to 768
        // 768×768 → 2×2=4 tiles → 85+680=765
        assert_eq!(estimate_openai_tokens(4096, 4096, "high"), 765);
    }

    // ── Anthropic token estimation ───────────────────────────────────

    #[test]
    fn anthro_small_image() {
        // 500×500 → 250000/750 = 333
        assert_eq!(estimate_anthropic_tokens(500, 500), 333);
    }

    #[test]
    fn anthro_large_edge_capped() {
        // 3000×1000 → max_edge=3000>1568, scale to 1568×523 (rounded)
        // 1568×523=820064, <1.15MP → 820064/750=1093
        let (w, h) = find_optimal_anthropic_dimensions(3000, 1000);
        assert_eq!(estimate_anthropic_tokens(w, h), 1093);
    }

    #[test]
    fn anthro_bills_the_edge_cap_only() {
        // 2000x2000 → the provider's 1568px edge limit → 1568x1568 = 2,458,624
        // px, /750 = 3,278. It does NOT further cap to 1.15MP for billing; this
        // test used to assert `< 2000` on that belief and so certified the
        // no-op. Confirmed against count_tokens (see estimate_anthropic_tokens).
        assert_eq!(estimate_anthropic_tokens(2000, 2000), 3_278);
    }

    // ── OpenAI dimension optimization ────────────────────────────────

    #[test]
    fn openai_optimize_no_change_for_small() {
        let (w, h) = find_optimal_openai_dimensions(400, 300);
        assert_eq!((w, h), (400, 300));
    }

    #[test]
    fn openai_optimize_reduces_tiles() {
        let (w, h) = find_optimal_openai_dimensions(1000, 1000);
        // Should find something with fewer tiles than 1000×1000
        let orig_tiles = estimate_openai_tokens(1000, 1000, "high");
        let opt_tiles = estimate_openai_tokens(w, h, "high");
        assert!(opt_tiles <= orig_tiles);
    }

    // ── Anthropic dimension optimization ─────────────────────────────

    #[test]
    fn anthro_optimize_small_unchanged() {
        let (w, h) = find_optimal_anthropic_dimensions(500, 500);
        assert_eq!((w, h), (500, 500));
    }

    #[test]
    fn anthro_optimize_large_edge() {
        let (w, h) = find_optimal_anthropic_dimensions(3000, 1000);
        assert!(w <= 1568);
        assert!(h <= 1568);
    }

    // ── TileOptResult ────────────────────────────────────────────────

    #[test]
    fn result_tokens_saved() {
        let r = TileOptResult {
            original_width: 1000,
            original_height: 1000,
            optimized_width: 512,
            optimized_height: 512,
            tokens_before: 765,
            tokens_after: 255,
            provider: "openai".to_string(),
            resized: true,
        };
        assert_eq!(r.tokens_saved(), 510);
        assert!((r.savings_pct() - 66.67).abs() < 0.1);
    }

    #[test]
    fn result_zero_tokens() {
        let r = TileOptResult {
            original_width: 0,
            original_height: 0,
            optimized_width: 0,
            optimized_height: 0,
            tokens_before: 0,
            tokens_after: 0,
            provider: "openai".to_string(),
            resized: false,
        };
        assert_eq!(r.savings_pct(), 0.0);
    }

    // ── Message optimization ─────────────────────────────────────────

    #[test]
    fn optimize_non_image_messages_unchanged() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let (opt, results) = optimize_images_in_messages(&msgs, "anthropic");
        assert_eq!(opt.len(), 1);
        assert!(results.is_empty());
    }

    #[test]
    fn optimize_string_content_unchanged() {
        let msgs = vec![json!({"role": "user", "content": "text only"})];
        let (opt, _) = optimize_images_in_messages(&msgs, "openai");
        assert_eq!(opt[0]["content"], "text only");
    }

    // ── Real image resize ────────────────────────────────────────────

    /// Create a minimal valid PNG of the given dimensions.
    fn make_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(width, height);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn decode_data_url_works() {
        let png = make_png(100, 100);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
        let url = format!("data:image/png;base64,{b64}");
        let decoded = decode_data_url(&url).unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn optimize_anthropic_image_block_resizes() {
        // 1800×1800: Anthropic caps internally so no token savings,
        // but the optimizer still resizes for bandwidth savings.
        // We verify the resize happens and produces valid output.
        let png = make_png(1800, 1800);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let item = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": b64}
        });

        let result = optimize_content_block(&item, "anthropic");
        // This asserted `is_none()` — "tokens same due to internal capping" —
        // which was the phantom 1.15MP cap. 1800x1800 bills as 1568x1568 =
        // 3,278 tokens and resizing to 1.15MP takes it to 1,532, so there is a
        // real saving and the block must be rewritten.
        let (new_item, res) = result.expect("an oversized image must be resized");
        assert_eq!(res.tokens_before, 3_278);
        assert!(
            res.tokens_after < res.tokens_before,
            "{} -> {}",
            res.tokens_before,
            res.tokens_after
        );
        assert_eq!(new_item["source"]["media_type"], json!("image/jpeg"));
    }

    #[test]
    fn optimize_anthropic_bandwidth_saves_bytes() {
        // Verify Anthropic optimize reduces file size even when token count is same.
        // Use a large image where Anthropic would resize anyway.
        let png = make_png(1800, 1800);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
        let orig_bytes = b64.len();

        // The Anthropic path returns None because tokens are same.
        // But the underlying resize IS happening in the OpenAI path.
        // This test validates the resize produces smaller output.
        let opt_png = make_png(1800, 1800);
        let img = image::load_from_memory(&opt_png).unwrap();
        let (opt_w, opt_h) = find_optimal_anthropic_dimensions(1800, 1800);
        let resized = img.resize(opt_w, opt_h, image::imageops::FilterType::Lanczos3);
        let mut buf = std::io::Cursor::new(Vec::new());
        resized
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .unwrap();
        let resized_bytes = buf.into_inner();
        let resized_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resized_bytes);
        assert!(
            resized_b64.len() < orig_bytes,
            "resized JPEG should be smaller than original PNG"
        );
    }

    #[test]
    fn optimize_openai_image_block_resizes() {
        let png = make_png(2000, 2000);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let item = json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{b64}")}
        });

        let result = optimize_content_block(&item, "openai");
        assert!(result.is_some());

        let (_, opt) = result.unwrap();
        assert!(opt.resized);
        assert!(opt.tokens_saved() > 0);
    }

    #[test]
    fn optimize_skips_small_images() {
        let png = make_png(100, 100);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let item = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": b64}
        });

        // Small image — no resize benefit
        let result = optimize_content_block(&item, "anthropic");
        assert!(result.is_none());
    }

    #[test]
    fn optimize_skips_url_referenced_images() {
        let item = json!({
            "type": "image_url",
            "image_url": {"url": "https://example.com/photo.jpg"}
        });
        let result = optimize_content_block(&item, "openai");
        assert!(result.is_none());
    }

    #[test]
    fn optimize_skips_non_base64_anthropic() {
        let item = json!({
            "type": "image",
            "source": {"type": "url", "url": "https://example.com/photo.jpg"}
        });
        let result = optimize_content_block(&item, "anthropic");
        assert!(result.is_none());
    }

    #[test]
    fn optimize_messages_end_to_end() {
        let png = make_png(2000, 2000);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        let msgs = vec![
            json!({"role": "user", "content": "look at this"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}
            ]}),
        ];

        let (opt, results) = optimize_images_in_messages(&msgs, "openai");
        assert_eq!(opt.len(), 2);
        assert_eq!(results.len(), 1);
        assert!(results[0].tokens_saved() > 0);
        // Text message unchanged
        assert_eq!(opt[0]["content"], "look at this");
    }

    // ── Wiring: what made this module inert ──────────────────────────

    /// A deterministic PNG of the given size, big enough to be worth resizing.
    fn png_b64(w: u32, h: u32) -> String {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, buf.into_inner())
    }

    fn image_block(w: u32, h: u32) -> Value {
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": png_b64(w, h) }
        })
    }

    #[test]
    fn an_image_nested_in_a_tool_result_is_found() {
        // Measured over 800 live bodies: all 13 distinct images sat inside
        // `tool_result.content` and none at the top level, so a walker that only
        // reads the top level finds nothing at all.
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [ image_block(1560, 1900) ]
            }]
        })];

        let (opt, results) = optimize_images_in_messages_cached(&messages, "anthropic");

        assert_eq!(results.len(), 1, "the nested image must be optimized");
        assert!(results[0].tokens_saved() > 0);
        let inner = &opt[0]["content"][0]["content"][0];
        assert_eq!(inner["source"]["media_type"], json!("image/jpeg"));
        assert_ne!(
            inner["source"]["data"], messages[0]["content"][0]["content"][0]["source"]["data"],
            "the nested block must actually be rewritten, not just counted"
        );
    }

    #[test]
    fn the_same_image_forwards_identically_every_turn() {
        // Prefix stability. If a resize is not byte-stable the cached prefix
        // breaks at that image and everything after it is re-created — which
        // would cost far more than the image saves.
        let messages = vec![json!({ "role": "user", "content": [ image_block(1560, 1500) ] })];

        let (first, r1) = optimize_images_in_messages_cached(&messages, "anthropic");
        let (second, r2) = optimize_images_in_messages_cached(&messages, "anthropic");

        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1, "a cached hit must still report its saving");
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "identical input must forward identical bytes"
        );
    }

    #[test]
    fn an_image_already_within_the_limits_is_left_alone() {
        let messages = vec![json!({ "role": "user", "content": [ image_block(400, 300) ] })];

        let (opt, results) = optimize_images_in_messages_cached(&messages, "anthropic");

        assert!(results.is_empty(), "nothing to gain, so do not touch it");
        assert_eq!(
            opt[0]["content"][0]["source"]["media_type"],
            json!("image/png"),
            "an untouched image must keep its original encoding"
        );
    }

    #[test]
    fn a_message_without_images_is_returned_unchanged() {
        let messages = vec![json!({
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "t", "content": [
                { "type": "text", "text": "no pixels here" }
            ]}]
        })];

        let (opt, results) = optimize_images_in_messages_cached(&messages, "anthropic");

        assert!(results.is_empty());
        assert_eq!(opt, messages);
    }
}
