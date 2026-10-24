//! The total parser: every string reads as a [`SearchQuery`].
//!
//! Two passes over the input's scalars. [`lex`] cuts it into tokens, reading
//! each operator's value as it goes and turning any operator it cannot read
//! into text. [`Parser`] then builds the tree from the tokens under the size
//! limits. Neither pass can fail; what either could not read is reported as a
//! [`ParseNotice`].

use super::ast::{
    Cmp, DateField, Expr, Filter, HasValue, IsValue, Keyword, NoticeKind, ParseNotice, SearchKind,
    SearchQuery, StateValue,
};
use super::date::{DateSpan, DateValue};
use super::{MAX_CLAUSES, MAX_DEPTH, MAX_INPUT, MAX_TEXT};

/// What [`super::parse_with_notices`] returns: the query, and what in the
/// input it read as text or left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// The query.
    pub query: SearchQuery,
    /// Why parts of the input were read as text or ignored, in input order.
    pub notices: Vec<ParseNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    Or,
    Minus,
    Leaf(Expr),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
}

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

/// A character a bare term cannot hold.
fn ends_bare(c: char) -> bool {
    is_ws(c) || matches!(c, '"' | '(' | ')')
}

fn notice(kind: NoticeKind, start: usize, end: usize) -> ParseNotice {
    let at = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    ParseNotice {
        kind,
        start: at(start),
        end: at(end),
    }
}

fn text_of(chars: &[char]) -> String {
    chars.iter().collect()
}

/// Read `"…"` from `at`, which holds the opening quote. Returns the text
/// inside, the index past the closing quote, and whether there was one.
fn quoted(chars: &[char], at: usize) -> (String, usize, bool) {
    let from = at + 1;
    match chars[from..].iter().position(|&c| c == '"') {
        Some(n) => (text_of(&chars[from..from + n]), from + n + 1, true),
        None => (text_of(&chars[from..]), chars.len(), false),
    }
}

/// The index past the bare run starting at `at`, which stops at whitespace,
/// a quote, a parenthesis, and, when `comma` is set, a comma.
fn bare_end(chars: &[char], at: usize, comma: bool) -> usize {
    chars[at..]
        .iter()
        .position(|&c| ends_bare(c) || (comma && c == ','))
        .map_or(chars.len(), |n| at + n)
}

/// One operator key the grammar names.
#[derive(Debug, Clone, Copy)]
enum Key {
    Stream,
    Context,
    Place,
    State,
    Kind,
    Is,
    Has,
    Priority,
    Date(DateField),
}

fn key_of(word: &str) -> Option<Key> {
    let lower = word.to_ascii_lowercase();
    Some(match lower.as_str() {
        "stream" => Key::Stream,
        "context" => Key::Context,
        "place" => Key::Place,
        "state" => Key::State,
        "kind" => Key::Kind,
        "is" => Key::Is,
        "has" => Key::Has,
        "priority" => Key::Priority,
        other => Key::Date(DateField::ALL.into_iter().find(|f| f.key() == other)?),
    })
}

/// The comparator at `at`, and its length.
fn cmp_at(chars: &[char], at: usize) -> Option<(Cmp, usize)> {
    let next = chars.get(at + 1).copied();
    match chars.get(at)? {
        ':' => Some((Cmp::Eq, 1)),
        '<' if next == Some('=') => Some((Cmp::Le, 2)),
        '>' if next == Some('=') => Some((Cmp::Ge, 2)),
        '<' => Some((Cmp::Lt, 1)),
        '>' => Some((Cmp::Gt, 1)),
        _ => None,
    }
}

/// A filter's value items: comma-separated, each bare or quoted. Returns the
/// items, the index past the value, and whether a quote was left open.
fn value_items(chars: &[char], mut at: usize) -> (Vec<String>, usize, bool) {
    let mut items = Vec::new();
    let mut open = false;
    loop {
        if chars.get(at) == Some(&'"') {
            let (text, next, closed) = quoted(chars, at);
            items.push(text);
            at = next;
            if !closed {
                open = true;
                break;
            }
        } else {
            let end = bare_end(chars, at, true);
            items.push(text_of(&chars[at..end]));
            at = end;
        }
        if chars.get(at) == Some(&',') {
            at += 1;
        } else {
            break;
        }
    }
    (items, at, open)
}

fn keywords<T: Keyword>(items: &[String]) -> Option<Vec<T>> {
    items.iter().map(|s| T::from_keyword(s)).collect()
}

fn names(items: &[String]) -> Option<Vec<String>> {
    items.iter().all(|s| !s.is_empty()).then(|| items.to_vec())
}

/// The filter `key cmp items` means, or `None` when the value is not one the
/// key takes.
fn filter_of(key: Key, cmp: Cmp, items: &[String]) -> Option<Filter> {
    let only_eq = |f: Filter| (cmp == Cmp::Eq).then_some(f);
    match key {
        Key::Stream => only_eq(Filter::Stream(names(items)?)),
        Key::Context => only_eq(Filter::Context(names(items)?)),
        Key::Place => only_eq(Filter::Place(names(items)?)),
        Key::State => only_eq(Filter::State(keywords::<StateValue>(items)?)),
        Key::Kind => only_eq(Filter::Kind(keywords::<SearchKind>(items)?)),
        Key::Is => only_eq(Filter::Is(keywords::<IsValue>(items)?)),
        Key::Has => only_eq(Filter::Has(keywords::<HasValue>(items)?)),
        Key::Priority => {
            if cmp != Cmp::Eq && items.len() != 1 {
                return None;
            }
            let values = items
                .iter()
                .map(|s| match s.as_bytes() {
                    [d @ b'1'..=b'5'] => Some(d - b'0'),
                    _ => None,
                })
                .collect::<Option<Vec<u8>>>()?;
            Some(Filter::Priority(cmp, values))
        }
        Key::Date(field) => {
            let [item] = items else { return None };
            let span = match item.split_once("..") {
                Some((a, b)) if cmp == Cmp::Eq => {
                    DateSpan::Range(DateValue::parse(a)?, DateValue::parse(b)?)
                }
                Some(_) => return None,
                None => DateSpan::One(DateValue::parse(item)?),
            };
            Some(Filter::Date(field, cmp, span))
        }
    }
}

/// The text of a token the lexer could not read as an operator, spelled so
/// it reads back as the same text: an open quote is closed.
fn raw_term(chars: &[char], start: usize, end: usize, open_quote: bool) -> Expr {
    let mut raw = text_of(&chars[start..end]);
    if open_quote {
        raw.push('"');
    }
    Expr::Term(raw)
}

/// Cut `chars` into tokens.
fn lex(chars: &[char], notices: &mut Vec<ParseNotice>) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_ws(c) {
            i += 1;
            continue;
        }
        let start = i;
        let push = |out: &mut Vec<Token>, tok: Tok| out.push(Token { tok, start });
        match c {
            '(' => {
                push(&mut out, Tok::LParen);
                i += 1;
            }
            ')' => {
                push(&mut out, Tok::RParen);
                i += 1;
            }
            '"' => {
                let (text, next, closed) = quoted(chars, i);
                if !closed {
                    notices.push(notice(NoticeKind::UnclosedQuote, start, next));
                }
                let text = text.trim();
                if text.is_empty() {
                    notices.push(notice(NoticeKind::EmptyPhrase, start, next));
                } else {
                    push(&mut out, Tok::Leaf(Expr::Phrase(text.to_owned())));
                }
                i = next;
            }
            '-' if chars.get(i + 1).is_some_and(|&n| !is_ws(n) && n != ')') => {
                push(&mut out, Tok::Minus);
                i += 1;
            }
            '#' | '@' if chars.get(i + 1).is_some_and(|&n| !ends_bare(n) || n == '"') => {
                let (name, next, open) = if chars[i + 1] == '"' {
                    let (text, next, closed) = quoted(chars, i + 1);
                    (text, next, !closed)
                } else {
                    let end = bare_end(chars, i + 1, false);
                    (text_of(&chars[i + 1..end]), end, false)
                };
                let leaf = if name.is_empty() {
                    notices.push(notice(NoticeKind::BadValue, start, next));
                    raw_term(chars, start, next, open)
                } else {
                    if open {
                        notices.push(notice(NoticeKind::UnclosedQuote, start, next));
                    }
                    Expr::Filter(if c == '#' {
                        Filter::Stream(vec![name])
                    } else {
                        Filter::Context(vec![name])
                    })
                };
                push(&mut out, Tok::Leaf(leaf));
                i = next;
            }
            '!' if matches!(chars.get(i + 1), Some('1'..='5'))
                && chars.get(i + 2).is_none_or(|&n| ends_bare(n)) =>
            {
                let d = chars[i + 1] as u8 - b'0';
                push(
                    &mut out,
                    Tok::Leaf(Expr::Filter(Filter::Priority(Cmp::Eq, vec![d]))),
                );
                i += 2;
            }
            _ => {
                let word_end = chars[i..]
                    .iter()
                    .position(|c| !c.is_ascii_alphabetic())
                    .map_or(chars.len(), |n| i + n);
                let word = text_of(&chars[i..word_end]);
                let operator = (word_end > i)
                    .then(|| key_of(&word))
                    .flatten()
                    .zip(cmp_at(chars, word_end));
                if let Some((key, (cmp, len))) = operator {
                    let (items, next, open) = value_items(chars, word_end + len);
                    let leaf = if let Some(f) = filter_of(key, cmp, &items) {
                        if open {
                            notices.push(notice(NoticeKind::UnclosedQuote, start, next));
                        }
                        Expr::Filter(f)
                    } else {
                        notices.push(notice(NoticeKind::BadValue, start, next));
                        raw_term(chars, start, next, open)
                    };
                    push(&mut out, Tok::Leaf(leaf));
                    i = next;
                    continue;
                }
                let end = bare_end(chars, i, false);
                if word_end > i && chars.get(word_end) == Some(&':') {
                    notices.push(notice(NoticeKind::UnknownKey, start, end));
                }
                let text = text_of(&chars[i..end]);
                if text == "OR" {
                    push(&mut out, Tok::Or);
                } else {
                    push(&mut out, Tok::Leaf(Expr::Term(text)));
                }
                i = end;
            }
        }
    }
    out
}

/// Why the parser stopped before the end of the tokens.
#[derive(Debug, Clone, Copy)]
struct Abort {
    kind: NoticeKind,
    /// The input index the rest starts at.
    start: usize,
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    leaves: usize,
    text: usize,
    abort: Option<Abort>,
    notices: Vec<ParseNotice>,
    len: usize,
}

/// What a leaf spends of the [`MAX_TEXT`] budget: the scalars of its text,
/// or of its canonical form for an operator. A function of the leaf alone,
/// so the canonical text of a query spends what the query did.
pub(super) fn leaf_cost(e: &Expr) -> usize {
    match e {
        Expr::Term(t) | Expr::Phrase(t) => t.chars().count(),
        other => other.to_string().chars().count(),
    }
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn start_of(&self, at: usize) -> usize {
        self.toks.get(at).map_or(self.len, |t| t.start)
    }

    fn and_expr(&mut self, depth: usize) -> Vec<Expr> {
        let mut items = Vec::new();
        while self.abort.is_none() {
            match self.peek() {
                None => break,
                Some(Tok::RParen) if depth > 0 => break,
                Some(Tok::RParen) => {
                    let at = self.start_of(self.pos);
                    self.notices
                        .push(notice(NoticeKind::StrayParen, at, at + 1));
                    self.pos += 1;
                }
                Some(_) => {
                    if let Some(e) = self.or_expr(depth) {
                        items.push(e);
                    }
                }
            }
        }
        items
    }

    fn or_expr(&mut self, depth: usize) -> Option<Expr> {
        let mut items: Vec<Expr> = self.unary(depth).into_iter().collect();
        while self.abort.is_none() && self.peek() == Some(&Tok::Or) {
            let after = self.toks.get(self.pos + 1).map(|t| &t.tok);
            if matches!(after, None | Some(Tok::RParen)) {
                break;
            }
            if items.is_empty() {
                break;
            }
            self.pos += 1;
            if let Some(e) = self.unary(depth) {
                items.push(e);
            }
        }
        match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Expr::Or(items)),
        }
    }

    fn unary(&mut self, depth: usize) -> Option<Expr> {
        let mut negated = false;
        while self.peek() == Some(&Tok::Minus) {
            negated = !negated;
            self.pos += 1;
        }
        let atom = self.atom(depth)?;
        Some(if negated {
            Expr::Not(Box::new(atom))
        } else {
            atom
        })
    }

    fn leaf(&mut self, e: Expr) -> Option<Expr> {
        let start = self.start_of(self.pos);
        if self.leaves >= MAX_CLAUSES {
            self.abort = Some(Abort {
                kind: NoticeKind::TooManyClauses,
                start,
            });
            return None;
        }
        let cost = leaf_cost(&e);
        if self.text + cost > MAX_TEXT {
            self.abort = Some(Abort {
                kind: NoticeKind::TooLong,
                start,
            });
            return None;
        }
        self.pos += 1;
        self.leaves += 1;
        self.text += cost;
        Some(e)
    }

    fn atom(&mut self, depth: usize) -> Option<Expr> {
        match self.peek()?.clone() {
            Tok::Leaf(e) => self.leaf(e),
            Tok::Or => {
                let at = self.start_of(self.pos);
                self.notices.push(notice(NoticeKind::LiteralOr, at, at + 2));
                self.leaf(Expr::Phrase("OR".to_owned()))
            }
            Tok::LParen => {
                let open = self.start_of(self.pos);
                if depth >= MAX_DEPTH {
                    self.abort = Some(Abort {
                        kind: NoticeKind::TooDeep,
                        start: open,
                    });
                    return None;
                }
                self.pos += 1;
                let items = self.and_expr(depth + 1);
                if self.abort.is_none() {
                    if self.peek() == Some(&Tok::RParen) {
                        self.pos += 1;
                    } else {
                        self.notices
                            .push(notice(NoticeKind::UnclosedGroup, open, self.len));
                    }
                }
                (!items.is_empty()).then_some(Expr::Group(items))
            }
            // A `-` is only lexed before a token, and `unary` takes every one.
            Tok::Minus | Tok::RParen => None,
        }
    }
}

/// The input past a limit, as one phrase's text: quotes dropped and
/// whitespace collapsed, at most `budget` scalars.
fn rest_text(chars: &[char], budget: usize) -> String {
    let joined = text_of(chars)
        .replace('"', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = joined.chars().take(budget).collect();
    cut.trim_end().to_owned()
}

pub(super) fn parse_with_notices(input: &str) -> Parsed {
    let mut chars: Vec<char> = input.chars().collect();
    let mut notices = Vec::new();
    if chars.len() > MAX_INPUT {
        notices.push(notice(NoticeKind::TooLong, MAX_INPUT, chars.len()));
        chars.truncate(MAX_INPUT);
    }
    let toks = lex(&chars, &mut notices);
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        leaves: 0,
        text: 0,
        abort: None,
        notices: Vec::new(),
        len: chars.len(),
    };
    let mut clauses = p.and_expr(0);
    if let Some(abort) = p.abort {
        notices.retain(|n| (n.start as usize) < abort.start);
        notices.push(notice(abort.kind, abort.start, chars.len()));
        if abort.kind != NoticeKind::TooLong {
            let rest = rest_text(&chars[abort.start..], MAX_TEXT - p.text);
            if !rest.is_empty() {
                clauses.push(Expr::Phrase(rest));
            }
        }
    }
    notices.extend(p.notices);
    notices.sort_by_key(|n| (n.start, n.end));
    Parsed {
        query: SearchQuery { clauses },
        notices,
    }
}
