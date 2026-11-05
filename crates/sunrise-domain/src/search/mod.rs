//! The search query language: a total parser, its canonical text, and the
//! text folding and highlighting the index and the results share.
//!
//! Implements `docs/08-features/search.md` §Query language, the decision of
//! [ADR-0052](../../../../docs/11-adr/0052-search-v2.md) §3: the grammar is
//! parsed here, once, in Rust, so every client and the CLI read a query the
//! same way and a saved search means the same thing on every device.
//!
//! - [`parse()`] is **total**. Every string reads as a [`SearchQuery`]: an
//!   unknown operator, a value an operator does not take, an unclosed quote
//!   or parenthesis, and anything past the size limits become text or are
//!   left out, and [`parse_with_notices`] says which and where.
//! - `Display` prints the **canonical** text, and
//!   `parse(&q.to_string()) == q` for every `q` the parser returns.
//! - Nothing here reaches FTS5. The core quotes every term it sends, so no
//!   query, however hostile, is an FTS5 syntax error.
//!
//! Names (`#work`) and dates (`due<fri`) are kept as typed; the core resolves
//! them when the query runs, against the vault's names and the reader's day.

mod ast;
mod date;
mod display;
mod parse;
mod text;

pub use ast::{
    Cmp, DateField, Expr, Filter, HasValue, IsValue, NoticeKind, ParseNotice, SearchKind,
    SearchQuery, StateValue,
};
pub use date::{DateSpan, DateValue, DayOfWeek, Days, OffsetUnit, MAX_OFFSET};
pub use parse::Parsed;
pub use text::{fold, highlight, is_cjk, match_ranges, snippet, Highlighted, SNIPPET_LEN};

/// The query text budget, in Unicode scalars: what the terms, phrases and
/// operators of one query may spell between them. Past it, the rest of the
/// input is left out with a [`NoticeKind::TooLong`].
pub const MAX_TEXT: usize = 1024;

/// The most terms, phrases and operators one query holds. Past it, the rest
/// of the input is read as one phrase, with a [`NoticeKind::TooManyClauses`].
pub const MAX_CLAUSES: usize = 32;

/// The deepest parentheses nest. A group past it, and the rest of the input,
/// is read as one phrase, with a [`NoticeKind::TooDeep`].
pub const MAX_DEPTH: usize = 8;

/// The most input the parser reads, in Unicode scalars, whatever it holds.
/// Far more than [`MAX_TEXT`] can spend, so it never cuts a canonical query;
/// it bounds the work a pasted megabyte of whitespace costs.
pub const MAX_INPUT: usize = 16 * 1024;

/// The grammar version a saved search records (`search.md` §Saved
/// searches). A device whose version is older parses what it knows and
/// reads the rest as text.
pub const GRAMMAR_V: u16 = 1;

/// Parse `input` as a query. Never fails; see the module docs.
#[must_use]
pub fn parse(input: &str) -> SearchQuery {
    parse::parse_with_notices(input).query
}

/// [`parse()`], and why parts of the input were read as text or left out.
#[must_use]
pub fn parse_with_notices(input: &str) -> Parsed {
    parse::parse_with_notices(input)
}

impl SearchQuery {
    /// Every term and phrase a hit is looked for by, in input order: those
    /// not negated. What a result highlights.
    #[must_use]
    pub fn positive_text(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a Expr, negated: bool, out: &mut Vec<&'a str>) {
            match e {
                Expr::Term(t) | Expr::Phrase(t) if !negated => out.push(t),
                Expr::Term(_) | Expr::Phrase(_) | Expr::Filter(_) => {}
                Expr::Not(inner) => walk(inner, !negated, out),
                Expr::Or(items) | Expr::Group(items) => {
                    for i in items {
                        walk(i, negated, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for c in &self.clauses {
            walk(c, false, &mut out);
        }
        out
    }

    /// The last leaf of the query, when it is a bare term: the word a user
    /// is still typing, which the field also matches as a prefix.
    #[must_use]
    pub fn last_term(&self) -> Option<&str> {
        fn last(e: &Expr) -> Option<&Expr> {
            match e {
                Expr::Not(_) => None,
                Expr::Or(items) | Expr::Group(items) => items.last().and_then(last),
                leaf => Some(leaf),
            }
        }
        match self.clauses.last().and_then(last)? {
            Expr::Term(t) => Some(t),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
