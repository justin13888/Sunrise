//! Folding text for the trigram index, and highlighting matches in it.
//!
//! The trigram index holds text the core folded before writing it
//! (ADR-0052 §2): compatibility-decomposed (NFKD), combining marks dropped,
//! lowercased. A query term is folded the same way before it reaches the
//! index, so `cafe` finds `Café` and `ＡＢＣ` finds `abc`.
//!
//! Highlights are computed here too, by folding both sides and mapping each
//! folded match back to the original text, so ranges are Unicode-scalar
//! offsets into what the client shows and no client re-tokenizes.

use serde::{Deserialize, Serialize};
use unicode_normalization::char::{decompose_compatible, is_combining_mark};

/// Text with the ranges of it that matched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Highlighted {
    /// The text, as shown.
    pub text: String,
    /// Matched ranges as Unicode-scalar offsets into `text`, half-open,
    /// sorted, and neither overlapping nor touching.
    pub ranges: Vec<(u32, u32)>,
}

/// The longest snippet [`snippet`] returns, in scalars.
pub const SNIPPET_LEN: usize = 160;

/// Fold `c` onto `out`: decomposed, marks dropped, lowercased.
fn fold_char(c: char, mut out: impl FnMut(char)) {
    for lower in c.to_lowercase() {
        decompose_compatible(lower, |d| {
            if !is_combining_mark(d) {
                for l in d.to_lowercase() {
                    out(l);
                }
            }
        });
    }
}

/// `text` folded as the trigram index holds it.
#[must_use]
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        fold_char(c, |d| out.push(d));
    }
    out
}

/// `text` folded, with the index of the original scalar each folded one came
/// from, and whether each original scalar folded to nothing (a combining
/// mark).
fn fold_mapped(text: &str) -> (Vec<char>, Vec<u32>, Vec<bool>) {
    let mut folded = Vec::with_capacity(text.len());
    let mut from = Vec::with_capacity(text.len());
    let mut vanished = Vec::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        let at = u32::try_from(i).unwrap_or(u32::MAX);
        let before = folded.len();
        fold_char(c, |d| {
            folded.push(d);
            from.push(at);
        });
        vanished.push(folded.len() == before);
    }
    (folded, from, vanished)
}

/// Every place one of `needles` occurs in `text`, matching folded text
/// against folded text, as sorted and merged scalar ranges into `text`.
/// Empty needles are skipped.
#[must_use]
pub fn match_ranges(text: &str, needles: &[&str]) -> Vec<(u32, u32)> {
    let (hay, from, vanished) = fold_mapped(text);
    // A match ends after the marks that follow its last letter, which fold
    // to nothing but are drawn on it.
    let end_after = |last: u32| {
        let mut end = last as usize + 1;
        while vanished.get(end) == Some(&true) {
            end += 1;
        }
        u32::try_from(end).unwrap_or(u32::MAX)
    };
    let mut ranges = Vec::new();
    for needle in needles {
        let n: Vec<char> = fold(needle).chars().collect();
        if n.is_empty() || n.len() > hay.len() {
            continue;
        }
        let mut i = 0;
        while i + n.len() <= hay.len() {
            if hay[i..i + n.len()] == n[..] {
                ranges.push((from[i], end_after(from[i + n.len() - 1])));
                i += n.len();
            } else {
                i += 1;
            }
        }
    }
    merge(ranges)
}

fn merge(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// `text` with every match of `needles` highlighted.
#[must_use]
pub fn highlight(text: &str, needles: &[&str]) -> Highlighted {
    Highlighted {
        text: text.to_owned(),
        ranges: match_ranges(text, needles),
    }
}

/// At most [`SNIPPET_LEN`] scalars of `body` around its first match, with
/// line breaks shown as spaces, or `None` when nothing in it matches.
#[must_use]
pub fn snippet(body: &str, needles: &[&str]) -> Option<Highlighted> {
    let ranges = match_ranges(body, needles);
    let first = ranges.first()?.0 as usize;
    let chars: Vec<char> = body.chars().collect();
    let len = chars.len();
    let mut start = first.saturating_sub(SNIPPET_LEN / 4);
    let end = (start + SNIPPET_LEN).min(len);
    if end - start < SNIPPET_LEN {
        start = end.saturating_sub(SNIPPET_LEN);
    }
    let text: String = chars[start..end]
        .iter()
        .map(|&c| if c.is_control() { ' ' } else { c })
        .collect();
    let (lo, hi) = (
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(end).unwrap_or(u32::MAX),
    );
    let ranges = ranges
        .into_iter()
        .filter(|&(s, e)| e > lo && s < hi)
        .map(|(s, e)| (s.max(lo) - lo, e.min(hi) - lo))
        .collect();
    Some(Highlighted { text, ranges })
}

/// Whether `c` is written in a script `unicode61` does not split into words:
/// Han, Hiragana, Katakana, Hangul and their extensions.
#[must_use]
pub fn is_cjk(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x11FF
        | 0x2E80..=0x2FDF
        | 0x3040..=0x30FF
        | 0x3100..=0x312F
        | 0x3130..=0x318F
        | 0x31A0..=0x31FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF
        | 0xFF66..=0xFF9F
        | 0x20000..=0x3FFFF)
}
