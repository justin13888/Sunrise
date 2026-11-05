//! Ranking a typed query against command names, for a command palette.
//!
//! A subsequence match, not a prefix one: people type `gti` for "Go to
//! Inbox". The ranking decides whether Return runs the command someone meant,
//! so it is pure and here rather than in a view. Ported from the Apple app's
//! `PaletteMatch` and `CommandPaletteModel` (`CommandPalette.swift`), with
//! their tests. Characters are Unicode scalars, lowercased; the Swift
//! original compared grapheme clusters, which only differs for titles no
//! command has.

/// How well `query` matches `title`: higher is better, `None` is no match.
///
/// Every query character must appear in order. Each one found scores 1, plus
/// 8 at the very start, 6 at the start of a word, and 4 right after the
/// previous match; the title's length costs one point per eight characters,
/// so of two equal matches the shorter name — the one the query described
/// more completely — wins. Whitespace in the query is ignored; an empty query
/// matches everything with 0.
#[must_use]
pub fn palette_score(query: &str, title: &str) -> Option<i64> {
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let haystack: Vec<char> = title.chars().flat_map(char::to_lowercase).collect();
    let mut score: i64 = 0;
    let mut cursor = 0;
    let mut last: Option<usize> = None;
    for c in needle {
        let found = cursor + haystack[cursor..].iter().position(|h| *h == c)?;
        if found == 0 {
            score += 8;
        } else if !haystack[found - 1].is_alphanumeric() {
            score += 6;
        }
        if last.is_some_and(|l| found == l + 1) {
            score += 4;
        }
        score += 1;
        last = Some(found);
        cursor = found + 1;
    }
    Some(score - i64::try_from(haystack.len() / 8).unwrap_or(i64::MAX))
}

/// The indices of `titles` that match `query`, best first.
///
/// Ties keep the catalogue's order, and an empty query keeps it whole — so
/// with nothing typed the commands read the way the cheat sheet groups them.
#[must_use]
pub fn palette_rank(query: &str, titles: &[&str]) -> Vec<usize> {
    let query = query.trim();
    let mut scored: Vec<(usize, i64)> = titles
        .iter()
        .enumerate()
        .filter_map(|(i, t)| palette_score(query, t).map(|s| (i, s)))
        .collect();
    if !query.is_empty() {
        // Stable: equal scores stay in catalogue order.
        scored.sort_by(|a, b| b.1.cmp(&a.1));
    }
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Move a highlight over `count` rows, clamped rather than wrapped: a palette
/// that wrapped from the last row to the first would move the highlight off
/// screen on a key that everywhere else means "further down".
#[must_use]
pub const fn palette_highlight(current: usize, count: usize, down: bool) -> usize {
    if count == 0 {
        0
    } else if down {
        let next = current.saturating_add(1);
        if next < count {
            next
        } else {
            count - 1
        }
    } else {
        current.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From `KeymapTests`: a tie goes to the shorter name, and a missing
    /// character is no match.
    #[test]
    fn a_tie_goes_to_the_shorter_name_and_a_missing_letter_is_no_match() {
        let short = palette_score("open", "Open").unwrap();
        let long = palette_score("open", "Open Something Else").unwrap();
        assert!(short > long);
        assert_eq!(palette_score("open", "Inbox"), None);
    }

    #[test]
    fn a_subsequence_matches_and_word_starts_score_higher() {
        assert!(palette_score("gti", "Go to Inbox").is_some());
        let initials = palette_score("gti", "Go to Inbox").unwrap();
        let buried = palette_score("gti", "Settings Item").unwrap_or(i64::MIN);
        assert!(initials > buried);
        assert_eq!(palette_score("  ", "Anything"), Some(0));
        assert_eq!(
            palette_score("O P", "Open Palette"),
            palette_score("op", "Open Palette")
        );
    }

    /// The exact arithmetic, so a port to another client can be checked
    /// against the same numbers: "op" in "Open" is start (8) + 1, then
    /// adjacent (4) + 1, minus 4 / 8 = 0.
    #[test]
    fn the_score_is_the_documented_sum() {
        assert_eq!(palette_score("op", "Open"), Some(14));
        // "i" at a word start (6 + 1) in a 11-character title (-1).
        assert_eq!(palette_score("i", "Go to Inbox"), Some(6));
    }

    #[test]
    fn ranking_is_best_first_with_ties_and_an_empty_query_in_catalogue_order() {
        let titles = ["Open Command Palette", "Go to Inbox", "Open", "Complete"];
        assert_eq!(palette_rank("", &titles), vec![0, 1, 2, 3]);
        assert_eq!(palette_rank("   ", &titles), vec![0, 1, 2, 3]);
        assert_eq!(palette_rank("open", &titles), vec![2, 0]);
        assert_eq!(palette_rank("zzz", &titles), Vec::<usize>::new());
        let same = ["Alpha", "Bravo", "Atlas"];
        assert_eq!(palette_rank("a", &same), vec![0, 2, 1]);
    }

    #[test]
    fn the_highlight_clamps_at_both_ends() {
        assert_eq!(palette_highlight(0, 0, true), 0);
        assert_eq!(palette_highlight(0, 3, true), 1);
        assert_eq!(palette_highlight(2, 3, true), 2);
        assert_eq!(palette_highlight(0, 3, false), 0);
        assert_eq!(palette_highlight(2, 3, false), 1);
        assert_eq!(palette_highlight(9, 3, true), 2, "a stale highlight clamps");
    }
}
