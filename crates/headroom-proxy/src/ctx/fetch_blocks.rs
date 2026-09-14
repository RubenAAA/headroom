//! Fetch block splitter + template/content classifier.
//!
//! Port of context-mode's `src/fetch/blocks.ts` (upstream `next`, `5b9c00c`).
//! The rule is: chrome repeats across pages of the same host, content does
//! not. A block is `template` only when its hash was already seen on a
//! *different* page of the same host, so the rule never guesses from the
//! shape of a single page.
//!
//! Labels only — never drops bytes: `reassemble(&split_blocks(x)) == x` for
//! every input. Byte counts below are UTF-8 bytes of trimmed text, matching
//! upstream's `Buffer.byteLength`.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A block's label. `template` blocks are stored whole but kept out of the
/// FTS index; `content` blocks are indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Content,
    Template,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Content => "content",
            BlockKind::Template => "template",
        }
    }

    pub fn from_label(s: &str) -> Self {
        if s == "template" {
            BlockKind::Template
        } else {
            BlockKind::Content
        }
    }
}

/// One block of a converted document. `raw` carries the block's source bytes
/// verbatim (including trailing blank-line separators); concatenating `raw`
/// over all blocks in ordinal order reproduces the input exactly.
#[derive(Debug, Clone)]
pub struct Block {
    pub ordinal: usize,
    pub raw: String,
    pub text: String,
    pub hash: String,
}

/// A [`Block`] with its [`BlockKind`] label.
#[derive(Debug, Clone)]
pub struct ClassifiedBlock {
    pub ordinal: usize,
    pub raw: String,
    pub text: String,
    pub hash: String,
    pub kind: BlockKind,
}

/// Outcome of [`classify_blocks`].
#[derive(Debug)]
pub struct ClassifyResult {
    pub blocks: Vec<ClassifiedBlock>,
    /// True when this host had no other recorded page, so every block was
    /// admitted as content on no evidence. The caller must re-run once a
    /// second page of the host lands.
    pub provisional: bool,
    /// True when every textual block already exists on other pages of the
    /// host: the response carried the site shell, not this page.
    pub all_template: bool,
    pub content_bytes: usize,
    pub template_bytes: usize,
}

/// ASCII whitespace, matching the six characters upstream's `isSpace` tests.
/// Deliberately not `char::is_whitespace`: Unicode spaces must not split or
/// trim blocks, or two renderings hash differently across implementations.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C' | '\x0B')
}

fn trim_edges(s: &str) -> &str {
    s.trim_matches(is_space)
}

fn starts_with(s: &str, prefix: &str) -> bool {
    s.starts_with(prefix)
}

/// Split text into lines keeping each line's `\n` terminator, so
/// concatenation reproduces the input exactly.
fn split_lines_keep_ends(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    // `split_inclusive` on a non-empty string never yields empty pieces, and
    // yields the unterminated tail as the final piece — exactly the contract.
    text.split_inclusive('\n').collect()
}

/// Split converted markdown into blocks. A boundary is a blank line or a
/// heading line outside a fenced code block; blank lines stay attached to the
/// block they follow. Fenced code is never split.
pub fn split_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    if markdown.is_empty() {
        return blocks;
    }

    let mut raw_parts: Vec<&str> = Vec::new();
    let mut has_text = false;
    let mut in_fence = false;
    let mut pending_break = false;

    let flush = |raw_parts: &mut Vec<&str>, has_text: &mut bool, blocks: &mut Vec<Block>| {
        if raw_parts.is_empty() {
            return;
        }
        let raw: String = raw_parts.concat();
        let text = trim_edges(&raw).to_string();
        let hash = hash_block_text(&text);
        blocks.push(Block {
            ordinal: blocks.len(),
            raw,
            text,
            hash,
        });
        raw_parts.clear();
        *has_text = false;
    };

    for line in split_lines_keep_ends(markdown) {
        let trimmed = trim_edges(line);
        let is_fence_marker = starts_with(trimmed, "```") || starts_with(trimmed, "~~~");
        let blank = !in_fence && trimmed.is_empty();
        let heading = !in_fence && !is_fence_marker && starts_with(trimmed, "#");

        // A new block starts at the first non-blank line after a blank run,
        // or at a heading — but only if the block being built already carries
        // text, so leading separators never produce an empty block.
        if !blank && (pending_break || heading) && has_text {
            flush(&mut raw_parts, &mut has_text, &mut blocks);
            pending_break = false;
        }

        if is_fence_marker {
            in_fence = !in_fence;
        }

        raw_parts.push(line);
        if !blank {
            has_text = true;
        }
        if blank {
            pending_break = true;
        }
    }
    flush(&mut raw_parts, &mut has_text, &mut blocks);
    blocks
}

/// Exact inverse of [`split_blocks`].
pub fn reassemble(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        out.push_str(&b.raw);
    }
    out
}

/// Normalise a block for cross-page comparison: lower-cased, every
/// whitespace run collapsed to a single space. Deliberately conservative —
/// no punctuation stripping, no token dropping. Under-matching leaks chrome
/// (recoverable, visible); over-matching hides content (a silent loss).
///
/// Case mapping matches JavaScript's locale-insensitive `toLowerCase`
/// (which upstream calls), including its one context-sensitive rule: a
/// capital sigma (Σ, U+03A3) lowers to final sigma (ς, U+03C2) at the end of
/// a word and to σ elsewhere. Rust's `char::to_lowercase` always yields σ,
/// so the sigma rule is applied explicitly here — otherwise Greek blocks
/// hash differently than the reference implementation. All other default
/// case mappings agree (the remaining conditional special-casings are
/// locale-specific and JS does not apply them either).
pub fn normalize_block_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut in_run = false;
    for (i, &c) in chars.iter().enumerate() {
        if is_space(c) {
            in_run = true;
            continue;
        }
        if in_run && !out.is_empty() {
            out.push(' ');
        }
        in_run = false;
        if c == 'Σ' {
            out.push(if sigma_is_final(&chars, i) {
                'ς'
            } else {
                'σ'
            });
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Skip table for the sigma-finality scans: characters the reference
/// implementation looks *through* when judging whether a capital sigma is
/// word-final. Measured exhaustively against the reference (`toLowerCase`
/// context behaviour) over every valid code point — full BMP plus full
/// astral range — so this is ground truth, not a property-table
/// approximation. It deliberately does NOT equal PropList Case_Ignorable:
/// the reference also skips modifier symbols and mid-letter punctuation
/// (`` ` `` `'` `.` `:` `^`, quotes, primes), while most punctuation, digits,
/// spaces and symbols stop the scan. Sorted, disjoint; searched by bisection.
/// Generated from probe data (see audit); do not hand-edit.
const SIGMA_SKIP: &[(u32, u32)] = &[
    (0x0027, 0x0027),
    (0x002E, 0x002E),
    (0x003A, 0x003A),
    (0x005E, 0x005E),
    (0x0060, 0x0060),
    (0x00A8, 0x00A8),
    (0x00AD, 0x00AD),
    (0x00AF, 0x00AF),
    (0x00B4, 0x00B4),
    (0x00B7, 0x00B8),
    (0x02B0, 0x036F),
    (0x0374, 0x0375),
    (0x037A, 0x037A),
    (0x0384, 0x0385),
    (0x0387, 0x0387),
    (0x0483, 0x0489),
    (0x0559, 0x0559),
    (0x055F, 0x055F),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x05F4, 0x05F4),
    (0x0600, 0x0605),
    (0x0610, 0x061A),
    (0x061C, 0x061C),
    (0x0640, 0x0640),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DD),
    (0x06DF, 0x06E8),
    (0x06EA, 0x06ED),
    (0x070F, 0x070F),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F5),
    (0x07FA, 0x07FA),
    (0x07FD, 0x07FD),
    (0x0816, 0x082D),
    (0x0859, 0x085B),
    (0x0888, 0x0888),
    (0x0890, 0x0891),
    (0x0898, 0x089F),
    (0x08C9, 0x0902),
    (0x093A, 0x093A),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0971, 0x0971),
    (0x0981, 0x0981),
    (0x09BC, 0x09BC),
    (0x09C1, 0x09C4),
    (0x09CD, 0x09CD),
    (0x09E2, 0x09E3),
    (0x09FE, 0x09FE),
    (0x0A01, 0x0A02),
    (0x0A3C, 0x0A3C),
    (0x0A41, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A82),
    (0x0ABC, 0x0ABC),
    (0x0AC1, 0x0AC5),
    (0x0AC7, 0x0AC8),
    (0x0ACD, 0x0ACD),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFF),
    (0x0B01, 0x0B01),
    (0x0B3C, 0x0B3C),
    (0x0B3F, 0x0B3F),
    (0x0B41, 0x0B44),
    (0x0B4D, 0x0B4D),
    (0x0B55, 0x0B56),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BC0, 0x0BC0),
    (0x0BCD, 0x0BCD),
    (0x0C00, 0x0C00),
    (0x0C04, 0x0C04),
    (0x0C3C, 0x0C3C),
    (0x0C3E, 0x0C40),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C81),
    (0x0CBC, 0x0CBC),
    (0x0CBF, 0x0CBF),
    (0x0CC6, 0x0CC6),
    (0x0CCC, 0x0CCD),
    (0x0CE2, 0x0CE3),
    (0x0D00, 0x0D01),
    (0x0D3B, 0x0D3C),
    (0x0D41, 0x0D44),
    (0x0D4D, 0x0D4D),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D81),
    (0x0DCA, 0x0DCA),
    (0x0DD2, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E46, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC6, 0x0EC6),
    (0x0EC8, 0x0ECE),
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F71, 0x0F7E),
    (0x0F80, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x0FC6, 0x0FC6),
    (0x102D, 0x1030),
    (0x1032, 0x1037),
    (0x1039, 0x103A),
    (0x103D, 0x103E),
    (0x1058, 0x1059),
    (0x105E, 0x1060),
    (0x1071, 0x1074),
    (0x1082, 0x1082),
    (0x1085, 0x1086),
    (0x108D, 0x108D),
    (0x109D, 0x109D),
    (0x10FC, 0x10FC),
    (0x135D, 0x135F),
    (0x1712, 0x1714),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17B5),
    (0x17B7, 0x17BD),
    (0x17C6, 0x17C6),
    (0x17C9, 0x17D3),
    (0x17D7, 0x17D7),
    (0x17DD, 0x17DD),
    (0x180B, 0x180F),
    (0x1843, 0x1843),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193B),
    (0x1A17, 0x1A18),
    (0x1A1B, 0x1A1B),
    (0x1A56, 0x1A56),
    (0x1A58, 0x1A5E),
    (0x1A60, 0x1A60),
    (0x1A62, 0x1A62),
    (0x1A65, 0x1A6C),
    (0x1A73, 0x1A7C),
    (0x1A7F, 0x1A7F),
    (0x1AA7, 0x1AA7),
    (0x1AB0, 0x1ACE),
    (0x1B00, 0x1B03),
    (0x1B34, 0x1B34),
    (0x1B36, 0x1B3A),
    (0x1B3C, 0x1B3C),
    (0x1B42, 0x1B42),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B81),
    (0x1BA2, 0x1BA5),
    (0x1BA8, 0x1BA9),
    (0x1BAB, 0x1BAD),
    (0x1BE6, 0x1BE6),
    (0x1BE8, 0x1BE9),
    (0x1BED, 0x1BED),
    (0x1BEF, 0x1BF1),
    (0x1C2C, 0x1C33),
    (0x1C36, 0x1C37),
    (0x1C78, 0x1C7D),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE0),
    (0x1CE2, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF8, 0x1CF9),
    (0x1D2C, 0x1D6A),
    (0x1D78, 0x1D78),
    (0x1D9B, 0x1DFF),
    (0x1FBD, 0x1FBD),
    (0x1FBF, 0x1FC1),
    (0x1FCD, 0x1FCF),
    (0x1FDD, 0x1FDF),
    (0x1FED, 0x1FEF),
    (0x1FFD, 0x1FFE),
    (0x200B, 0x200F),
    (0x2018, 0x2019),
    (0x2024, 0x2024),
    (0x2027, 0x2027),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0x2071, 0x2071),
    (0x207F, 0x207F),
    (0x2090, 0x209C),
    (0x20D0, 0x20F0),
    (0x2C7C, 0x2C7D),
    (0x2CEF, 0x2CF1),
    (0x2D6F, 0x2D6F),
    (0x2D7F, 0x2D7F),
    (0x2DE0, 0x2DFF),
    (0x2E2F, 0x2E2F),
    (0x3005, 0x3005),
    (0x302A, 0x302D),
    (0x3031, 0x3035),
    (0x303B, 0x303B),
    (0x3099, 0x309E),
    (0x30FC, 0x30FE),
    (0xA015, 0xA015),
    (0xA4F8, 0xA4FD),
    (0xA60C, 0xA60C),
    (0xA66F, 0xA672),
    (0xA674, 0xA67D),
    (0xA67F, 0xA67F),
    (0xA69C, 0xA69F),
    (0xA6F0, 0xA6F1),
    (0xA700, 0xA721),
    (0xA770, 0xA770),
    (0xA788, 0xA78A),
    (0xA7F2, 0xA7F4),
    (0xA7F8, 0xA7F9),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA825, 0xA826),
    (0xA82C, 0xA82C),
    (0xA8C4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA951),
    (0xA980, 0xA982),
    (0xA9B3, 0xA9B3),
    (0xA9B6, 0xA9B9),
    (0xA9BC, 0xA9BD),
    (0xA9CF, 0xA9CF),
    (0xA9E5, 0xA9E6),
    (0xAA29, 0xAA2E),
    (0xAA31, 0xAA32),
    (0xAA35, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4C),
    (0xAA70, 0xAA70),
    (0xAA7C, 0xAA7C),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAADD, 0xAADD),
    (0xAAEC, 0xAAED),
    (0xAAF3, 0xAAF4),
    (0xAAF6, 0xAAF6),
    (0xAB5B, 0xAB5F),
    (0xAB69, 0xAB6B),
    (0xABE5, 0xABE5),
    (0xABE8, 0xABE8),
    (0xABED, 0xABED),
    (0xFB1E, 0xFB1E),
    (0xFBB2, 0xFBC2),
    (0xFE00, 0xFE0F),
    (0xFE13, 0xFE13),
    (0xFE20, 0xFE2F),
    (0xFE52, 0xFE52),
    (0xFE55, 0xFE55),
    (0xFEFF, 0xFEFF),
    (0xFF07, 0xFF07),
    (0xFF0E, 0xFF0E),
    (0xFF1A, 0xFF1A),
    (0xFF3E, 0xFF3E),
    (0xFF40, 0xFF40),
    (0xFF70, 0xFF70),
    (0xFF9E, 0xFF9F),
    (0xFFE3, 0xFFE3),
    (0xFFF9, 0xFFFB),
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10780, 0x10785),
    (0x10787, 0x107B0),
    (0x107B2, 0x107BA),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10EAB, 0x10EAC),
    (0x10EFD, 0x10EFF),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11001, 0x11001),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11081),
    (0x110B3, 0x110B6),
    (0x110B9, 0x110BA),
    (0x110BD, 0x110BD),
    (0x110C2, 0x110C2),
    (0x110CD, 0x110CD),
    (0x11100, 0x11102),
    (0x11127, 0x1112B),
    (0x1112D, 0x11134),
    (0x11173, 0x11173),
    (0x11180, 0x11181),
    (0x111B6, 0x111BE),
    (0x111C9, 0x111CC),
    (0x111CF, 0x111CF),
    (0x1122F, 0x11231),
    (0x11234, 0x11234),
    (0x11236, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112DF),
    (0x112E3, 0x112EA),
    (0x11300, 0x11301),
    (0x1133B, 0x1133C),
    (0x11340, 0x11340),
    (0x11366, 0x1136C),
    (0x11370, 0x11374),
    (0x11438, 0x1143F),
    (0x11442, 0x11444),
    (0x11446, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B3, 0x114B8),
    (0x114BA, 0x114BA),
    (0x114BF, 0x114C0),
    (0x114C2, 0x114C3),
    (0x115B2, 0x115B5),
    (0x115BC, 0x115BD),
    (0x115BF, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11633, 0x1163A),
    (0x1163D, 0x1163D),
    (0x1163F, 0x11640),
    (0x116AB, 0x116AB),
    (0x116AD, 0x116AD),
    (0x116B0, 0x116B5),
    (0x116B7, 0x116B7),
    (0x1171D, 0x1171F),
    (0x11722, 0x11725),
    (0x11727, 0x1172B),
    (0x1182F, 0x11837),
    (0x11839, 0x1183A),
    (0x1193B, 0x1193C),
    (0x1193E, 0x1193E),
    (0x11943, 0x11943),
    (0x119D4, 0x119D7),
    (0x119DA, 0x119DB),
    (0x119E0, 0x119E0),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A38),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A56),
    (0x11A59, 0x11A5B),
    (0x11A8A, 0x11A96),
    (0x11A98, 0x11A99),
    (0x11C30, 0x11C36),
    (0x11C38, 0x11C3D),
    (0x11C3F, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CAA, 0x11CB0),
    (0x11CB2, 0x11CB3),
    (0x11CB5, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D90, 0x11D91),
    (0x11D95, 0x11D95),
    (0x11D97, 0x11D97),
    (0x11EF3, 0x11EF4),
    (0x11F00, 0x11F01),
    (0x11F36, 0x11F3A),
    (0x11F40, 0x11F40),
    (0x11F42, 0x11F42),
    (0x13430, 0x13440),
    (0x13447, 0x13455),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16B40, 0x16B43),
    (0x16F4F, 0x16F4F),
    (0x16F8F, 0x16F9F),
    (0x16FE0, 0x16FE1),
    (0x16FE3, 0x16FE4),
    (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB),
    (0x1AFFD, 0x1AFFE),
    (0x1BC9D, 0x1BC9E),
    (0x1BCA0, 0x1BCA3),
    (0x1CF00, 0x1CF2D),
    (0x1CF30, 0x1CF46),
    (0x1D167, 0x1D169),
    (0x1D173, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DA9F),
    (0x1DAA1, 0x1DAAF),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E030, 0x1E06D),
    (0x1E08F, 0x1E08F),
    (0x1E130, 0x1E13D),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E4EB, 0x1E4EF),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94B),
    (0x1F3FB, 0x1F3FF),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xE0100, 0xE01EF),
];

/// "Cased" for sigma-finality: what the reference implementation treats as
/// a cased neighbour. That is the Lowercase/Uppercase properties (Rust's
/// `is_lowercase`/`is_uppercase`) PLUS characters that carry case mappings
/// without the properties — titlecase (Lt), Roman numerals (Nl), circled and
/// squared Latin (So) and the ordinal indicators (Lo) — MINUS the skip table
/// below. The subtraction is load-bearing, not belt-and-braces: lowercase
/// modifier letters (e.g. U+02B0 ʰ) have the Lowercase property but the
/// reference looks *through* them, so they must not count as cased here.
/// Enumerated from the same exhaustive probe that produced [`SIGMA_SKIP`].
fn is_cased(c: char) -> bool {
    (c.is_lowercase()
        || c.is_uppercase()
        || matches!(c,
            '\u{AA}' | '\u{BA}'
            | '\u{1C5}' | '\u{1C8}' | '\u{1CB}' | '\u{1F2}'
            | '\u{1F88}'..='\u{1F8F}'
            | '\u{1F98}'..='\u{1F9F}'
            | '\u{1FA8}'..='\u{1FAF}'
            | '\u{1FBC}' | '\u{1FCC}' | '\u{1FFC}'
            | '\u{2160}'..='\u{217F}'
            | '\u{24B6}'..='\u{24E9}'
            | '\u{1F130}'..='\u{1F149}'
            | '\u{1F150}'..='\u{1F169}'
            | '\u{1F170}'..='\u{1F189}'))
        && !is_sigma_transparent(c)
}

/// Whether the scans look through `c` when judging sigma finality.
/// Table lookup — see [`SIGMA_SKIP`]. [`is_cased`] excludes this set, so a
/// char is never both skipped and cased and the check order in the scans
/// does not matter.
fn is_sigma_transparent(c: char) -> bool {
    let n = c as u32;
    SIGMA_SKIP
        .binary_search_by(|&(lo, hi)| {
            if n < lo {
                std::cmp::Ordering::Greater
            } else if n > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Unicode Final_Sigma: Σ is word-final iff the nearest preceding
/// non-ignorable char is cased and the nearest following non-ignorable char
/// is not cased (or nothing follows).
fn sigma_is_final(chars: &[char], i: usize) -> bool {
    let mut before_cased = false;
    for &c in chars[..i].iter().rev() {
        if is_sigma_transparent(c) {
            continue;
        }
        before_cased = is_cased(c);
        break;
    }
    if !before_cased {
        return false;
    }
    for &c in &chars[i + 1..] {
        if is_sigma_transparent(c) {
            continue;
        }
        return !is_cased(c);
    }
    true
}

/// Full SHA-256 hex digest of the normalised block. Never shortened.
pub fn hash_block_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_block_text(text).as_bytes());
    hex::encode(hasher.finalize())
}

/// Classify every block of one page.
///
/// `other_page_count` maps a block hash to the number of *distinct other*
/// pages of this host carrying it (must exclude the page being classified,
/// or a page compared against itself marks its own article as template).
/// `host_page_count` is the distinct pages already recorded for this host,
/// excluding the current one. `authored` is set when the site served the
/// document as machine-readable markdown: no chrome to classify, every block
/// is content and the result is never provisional.
pub fn classify_blocks(
    blocks: Vec<Block>,
    other_page_count: &HashMap<String, usize>,
    host_page_count: usize,
    authored: bool,
) -> ClassifyResult {
    let cold_start = !authored && host_page_count < 1;

    let mut classified = Vec::with_capacity(blocks.len());
    let mut content_bytes = 0usize;
    let mut template_bytes = 0usize;
    let mut saw_content = false;
    let mut saw_text_block = false;

    for b in blocks {
        // A whitespace-only separator carries no information either way; keep
        // it as content so reassembly of the content stream stays readable.
        let textual = !b.text.is_empty();
        if textual {
            saw_text_block = true;
        }
        let repeated = textual
            && !authored
            && !cold_start
            && other_page_count.get(&b.hash).copied().unwrap_or(0) >= 1;
        let kind = if repeated {
            BlockKind::Template
        } else {
            BlockKind::Content
        };
        if kind == BlockKind::Content {
            if textual {
                saw_content = true;
            }
            content_bytes += b.text.len();
        } else {
            template_bytes += b.text.len();
        }
        classified.push(ClassifiedBlock {
            ordinal: b.ordinal,
            raw: b.raw,
            text: b.text,
            hash: b.hash,
            kind,
        });
    }

    ClassifyResult {
        blocks: classified,
        provisional: cold_start,
        // Every textual block of this page already exists on other pages of
        // this host: the response carried no page-specific content at all.
        all_template: saw_text_block && !saw_content,
        content_bytes,
        template_bytes,
    }
}

/// The text that goes to the search index: `content` blocks in document
/// order. Template blocks are left out of the index, not deleted — the caller
/// has already stored every block verbatim.
pub fn content_text(blocks: &[ClassifiedBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if b.kind == BlockKind::Content {
            out.push_str(&b.raw);
        }
    }
    trim_edges(&out).to_string()
}

/// The template stream in document order. Stored and retrievable, never
/// indexed.
pub fn template_text(blocks: &[ClassifiedBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if b.kind == BlockKind::Template {
            out.push_str(&b.raw);
        }
    }
    trim_edges(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn reassemble_is_exact_inverse() {
        for doc in [
            "",
            "# Title\n\nBody text.\n",
            "line one\r\nline two\r\n",
            "# A\n\n```js\ncode\n\nblank kept\n```\n\n# B\ntail without newline",
            "fenced ~~~\n~~~\ncode # not a heading\n~~~\n",
            "unicode: héllo wörld ✓\n\nsecond block  \n",
            "\n\n\n# Leading blanks\n\nbody\n\n\n",
        ] {
            let blocks = split_blocks(doc);
            assert_eq!(reassemble(&blocks), doc, "doc: {doc:?}");
        }
    }

    #[test]
    fn headings_and_fences_split_as_upstream() {
        let blocks = split_blocks("# A\n\ntext\n\n# B\n\nmore\n");
        assert_eq!(blocks.len(), 4);
        let blocks = split_blocks("# A\n\n```\n# not a heading\n\nblank kept\n```\n\ntail\n");
        // Fenced region (including its blank line and #-line) stays one unit.
        assert_eq!(blocks.len(), 3);
        assert!(blocks[1].raw.contains("# not a heading"));
    }

    #[test]
    fn hash_is_full_sha256_of_normalised_text() {
        // sha256("hello world").
        assert_eq!(
            hash_block_text("hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        // Case, indentation and wrapping collapse; punctuation does not.
        assert_eq!(
            hash_block_text("  Hello   WORLD\n"),
            hash_block_text("hello world")
        );
        assert_ne!(
            hash_block_text("hello world!"),
            hash_block_text("hello world")
        );
    }

    #[test]
    fn sigma_finality_matches_javascript_lowercase() {
        // V8 applies context-sensitive sigma lowering: word-final Σ → ς,
        // elsewhere → σ. Vectors verified against node ("ΟΣ".toLowerCase()).
        assert_eq!(normalize_block_text("ΟΣ"), "ος");
        assert_eq!(normalize_block_text("ΟΣ ΣΙΣ"), "ος σις");
        assert_eq!(normalize_block_text("ΣΙΣ"), "σις");
        assert_eq!(normalize_block_text("ΟΔΥΣΣΕΎΣ"), "οδυσσεύς");
        // A combining mark between letters does not break the word for
        // finality (mark is case-ignorable); the normalizer never composes.
        assert_eq!(normalize_block_text("Ο\u{301}Σ"), "ο\u{301}ς");
        assert_eq!(
            hash_block_text("ΟΣ ΣΙΣ~~~~"),
            "12586b2891c79ba17d9aec747eb3b66cf91bf082f47771dc966a8ea994db1874"
        );
    }

    #[test]
    fn sigma_tables_stay_consistent() {
        // SIGMA_SKIP must be sorted and disjoint (bisection relies on it),
        // and must never contain a cased char (cased neighbours resolve
        // before the skip check runs, so membership would shadow them).
        // The count locks the generated table against silent truncation.
        for pair in SIGMA_SKIP.windows(2) {
            assert!(
                pair[0].1 < pair[1].0,
                "skip ranges overlap or touch: {pair:?}"
            );
        }
        let mut covered = 0usize;
        for &(lo, hi) in SIGMA_SKIP {
            for n in lo..=hi {
                let c = char::from_u32(n).expect("table holds valid chars");
                assert!(!is_cased(c), "skip table shadows cased U+{n:04X}");
                covered += 1;
            }
        }
        assert_eq!(covered, 2707, "skip table lost entries");
    }

    #[test]
    fn cold_start_admits_everything_as_provisional() {
        let blocks = split_blocks("# Nav\n\n# Article\n");
        let result = classify_blocks(blocks, &HashMap::new(), 0, false);
        assert!(result.provisional);
        assert!(!result.all_template);
        assert!(result.blocks.iter().all(|b| b.kind == BlockKind::Content));
    }

    #[test]
    fn repeat_across_pages_marks_template_but_never_self() {
        let doc_a = "# Nav\n\nArticle A\n";
        let doc_b = "# Nav\n\nArticle B\n";
        let a = split_blocks(doc_a);
        let nav_hash = a[0].hash.clone();

        // Second page of the host: the shared nav is template, the article is
        // content.
        let mut counts = HashMap::new();
        counts.insert(nav_hash.clone(), 1);
        let result = classify_blocks(split_blocks(doc_b), &counts, 1, false);
        assert!(!result.provisional);
        let kinds: HashMap<_, _> = result
            .blocks
            .iter()
            .map(|b| (b.text.clone(), b.kind))
            .collect();
        assert_eq!(
            kinds.get("# Nav"),
            Some(&BlockKind::Template),
            "shared nav must be template"
        );
        assert_eq!(
            kinds.get("Article B"),
            Some(&BlockKind::Content),
            "unique article must be content"
        );

        // A page whose every textual block repeats is all-template (a shell).
        let mut all = HashMap::new();
        for b in split_blocks(doc_b) {
            all.insert(b.hash.clone(), 1);
        }
        // …except the page must be compared against OTHER pages: with an
        // empty comparison set (self excluded) nothing repeats.
        let self_only = classify_blocks(split_blocks(doc_b), &HashMap::new(), 1, false);
        assert!(!self_only.all_template);
        let shell = classify_blocks(split_blocks(doc_b), &all, 1, false);
        assert!(shell.all_template);
    }

    #[test]
    fn authored_markdown_skips_classification() {
        let blocks = split_blocks("# Nav\n\n# Article\n");
        let mut counts = HashMap::new();
        for b in &blocks {
            counts.insert(b.hash.clone(), 5);
        }
        let result = classify_blocks(blocks, &counts, 3, true);
        assert!(!result.provisional);
        assert!(!result.all_template);
        assert!(result.blocks.iter().all(|b| b.kind == BlockKind::Content));
    }

    #[test]
    fn content_text_keeps_order_and_drops_template() {
        let doc = "# Nav\n\nArticle\n";
        let mut counts = HashMap::new();
        counts.insert(split_blocks(doc)[0].hash.clone(), 1);
        let result = classify_blocks(split_blocks(doc), &counts, 1, false);
        assert_eq!(content_text(&result.blocks), "Article");
        assert_eq!(template_text(&result.blocks), "# Nav");
    }
}
