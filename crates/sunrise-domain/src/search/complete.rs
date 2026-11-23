//! Operator autocomplete: the ways to finish the word being typed at the end
//! of a query (`docs/08-features/search.md` §Operators, "The field
//! autocompletes keys, values and names").
//!
//! Pure, like the parser: the caller passes the field's text and the names
//! the vault holds, and gets back each suggestion as the whole field after
//! accepting it. The vocabulary is the parser's own, so a suggestion can
//! never be a spelling the parser reads as text. A client localizes the
//! description from [`CompletionKind`]; the keywords themselves are ASCII
//! English on every locale.

use super::ast::{DateField, HasValue, IsValue, Keyword, SearchKind, StateValue};
use super::display::{list_name, shorthand_name};

/// The most suggestions one call returns.
pub const MAX_COMPLETIONS: usize = 8;

/// The operator keys, in the order the grammar lists them.
const KEYS: [&str; 14] = [
    "stream", "context", "state", "kind", "is", "has", "priority", "planned", "target", "hard",
    "due", "created", "done", "place",
];

/// The date words a date operator suggests. Offsets and absolute dates are
/// typed, not suggested.
const DATE_WORDS: [&str; 16] = [
    "today",
    "tomorrow",
    "yesterday",
    "this-week",
    "next-week",
    "last-week",
    "this-month",
    "next-month",
    "last-month",
    "mon",
    "tue",
    "wed",
    "thu",
    "fri",
    "sat",
    "sun",
];

const PRIORITIES: [&str; 5] = ["1", "2", "3", "4", "5"];

/// The names a query can name, as the vault holds them now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionNames {
    /// Every stream's name.
    pub streams: Vec<String>,
    /// Every context's name.
    pub contexts: Vec<String>,
}

/// What a suggestion is: what a client describes it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    /// An operator key, `key` being its keyword (`stream`, `is`, `due`, …).
    Key(&'static str),
    /// A value of an operator: `key` is the operator's keyword, or `date`
    /// for any date operator, and `value` the value's keyword.
    Value {
        /// The operator.
        key: &'static str,
        /// The value.
        value: &'static str,
    },
    /// A stream's name, after `#` or `stream:`.
    Stream,
    /// A context's name, after `@` or `context:`.
    Context,
}

/// One way to finish the word being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The whole query after accepting it.
    pub text: String,
    /// The operator, value or name as it will read in the query.
    pub label: String,
    /// What it is.
    pub kind: CompletionKind,
}

/// The ways to finish the word at the end of `input`, best first, at most
/// [`MAX_COMPLETIONS`]. Empty when the input ends in whitespace (nothing is
/// being typed) or the word is already whole.
///
/// A key completes to `key:` with nothing after it, so the value is typed or
/// suggested next; a value or a name completes with a space after it, so the
/// next word can start at once.
#[must_use]
pub fn complete(input: &str, names: &CompletionNames) -> Vec<Completion> {
    let chars: Vec<char> = input.chars().collect();
    if chars.last().is_none_or(|c| c.is_whitespace()) {
        return Vec::new();
    }
    let start = word_start(&chars);
    let negated = chars.get(start) == Some(&'-');
    let word_from = if negated { start + 1 } else { start };
    let word: String = chars[word_from..].iter().collect();
    let head: String = chars[..word_from].iter().collect();

    let mut out = Vec::new();
    if let Some(rest) = word.strip_prefix('#') {
        shorthand(
            &head,
            '#',
            rest,
            &names.streams,
            CompletionKind::Stream,
            &mut out,
        );
    } else if let Some(rest) = word.strip_prefix('@') {
        shorthand(
            &head,
            '@',
            rest,
            &names.contexts,
            CompletionKind::Context,
            &mut out,
        );
    } else if word == "!" {
        for p in PRIORITIES {
            push(
                &mut out,
                &head,
                &format!("!{p}"),
                CompletionKind::Value {
                    key: "priority",
                    value: p,
                },
            );
        }
    } else if let Some((key, cmp_end)) = operator(&word) {
        let value: String = word.chars().skip(cmp_end).collect();
        let typed: String = word.chars().take(cmp_end).collect();
        values(&format!("{head}{typed}"), key, &value, names, &mut out);
    } else if !word.contains([':', '<', '>', '"']) {
        let lower = word.to_lowercase();
        for key in KEYS {
            if key.starts_with(&lower) && key != lower {
                out.push(Completion {
                    text: format!("{head}{key}:"),
                    label: format!("{key}:"),
                    kind: CompletionKind::Key(key),
                });
            }
        }
    }
    out.truncate(MAX_COMPLETIONS);
    out
}

/// What a view the search field is opened from is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// A stream's list.
    Stream,
    /// A context's list.
    Context,
}

/// The token the field is prefilled with when search is opened from a
/// stream's or a context's list (`docs/08-features/search.md` §Scope):
/// `#work ` or `@errands `, the name quoted as the canonical text quotes it,
/// and a space after it so the next word can be typed at once.
#[must_use]
pub fn scope_token(scope: Scope, name: &str) -> String {
    let sigil = match scope {
        Scope::Stream => '#',
        Scope::Context => '@',
    };
    format!("{sigil}{} ", shorthand_name(name))
}

/// Where the word being typed starts: after the last whitespace or `(`, or,
/// inside a quote left open, at the start of the word the quote opened in.
fn word_start(chars: &[char]) -> usize {
    let quotes: Vec<usize> = (0..chars.len()).filter(|&i| chars[i] == '"').collect();
    let scan_from = if quotes.len() % 2 == 1 {
        quotes[quotes.len() - 1]
    } else {
        chars.len()
    };
    chars[..scan_from]
        .iter()
        .rposition(|&c| c.is_whitespace() || c == '(')
        .map_or(0, |i| i + 1)
}

/// `key` and the index past its comparator, when `word` opens with an
/// operator key and a comparator.
fn operator(word: &str) -> Option<(&'static str, usize)> {
    let chars: Vec<char> = word.chars().collect();
    let at = chars.iter().position(|&c| matches!(c, ':' | '<' | '>'))?;
    let typed: String = chars[..at].iter().collect::<String>().to_ascii_lowercase();
    let key = KEYS.into_iter().find(|k| *k == typed)?;
    let len = if chars[at] != ':' && chars.get(at + 1) == Some(&'=') {
        2
    } else {
        1
    };
    let is_eq = chars[at] == ':';
    // `state<`, `kind>`: only the date operators and priority compare.
    let compares = key == "priority" || DateField::ALL.iter().any(|f| f.key() == key);
    (is_eq || compares).then_some((key, at + len))
}

/// The names after `#` or `@`.
fn shorthand(
    head: &str,
    sigil: char,
    rest: &str,
    names: &[String],
    kind: CompletionKind,
    out: &mut Vec<Completion>,
) {
    let typed = rest.strip_prefix('"').unwrap_or(rest).to_lowercase();
    for name in sorted(names) {
        let label = format!("{sigil}{}", shorthand_name(name));
        if name.to_lowercase().starts_with(&typed) && label != format!("{sigil}{rest}") {
            out.push(Completion {
                text: format!("{head}{label} "),
                label,
                kind,
            });
        }
    }
}

/// The values of `key` that finish `value`, the item after the last comma.
fn values(
    head: &str,
    key: &'static str,
    value: &str,
    names: &CompletionNames,
    out: &mut Vec<Completion>,
) {
    let (head, item) = match value.rsplit_once(',') {
        Some((done, item)) => (format!("{head}{done},"), item),
        None => (head.to_owned(), value),
    };
    let typed = item.strip_prefix('"').unwrap_or(item).to_lowercase();
    let mut offer = |label: String, kind: CompletionKind| {
        if label.to_lowercase().trim_matches('"').starts_with(&typed) && label != item {
            push(out, &head, &label, kind);
        }
    };
    let words = |all: &[&'static str], offer: &mut dyn FnMut(String, CompletionKind)| {
        for &v in all {
            offer(v.to_owned(), CompletionKind::Value { key, value: v });
        }
    };
    match key {
        "stream" => {
            for n in sorted(&names.streams) {
                offer(list_name(n), CompletionKind::Stream);
            }
        }
        "context" => {
            for n in sorted(&names.contexts) {
                offer(list_name(n), CompletionKind::Context);
            }
        }
        "state" => words(&keywords::<StateValue>(), &mut offer),
        "kind" => words(&keywords::<SearchKind>(), &mut offer),
        "is" => words(&keywords::<IsValue>(), &mut offer),
        "has" => words(&keywords::<HasValue>(), &mut offer),
        "priority" => words(&PRIORITIES, &mut offer),
        "place" => {}
        _ => {
            for &v in &DATE_WORDS {
                offer(
                    v.to_owned(),
                    CompletionKind::Value {
                        key: "date",
                        value: v,
                    },
                );
            }
        }
    }
}

fn keywords<T: Keyword>() -> Vec<&'static str> {
    T::ALL.iter().map(|v| v.keyword()).collect()
}

fn sorted(names: &[String]) -> Vec<&String> {
    let mut v: Vec<&String> = names.iter().collect();
    v.sort_by_key(|n| n.to_lowercase());
    v.dedup();
    v
}

fn push(out: &mut Vec<Completion>, head: &str, label: &str, kind: CompletionKind) {
    out.push(Completion {
        text: format!("{head}{label} "),
        label: label.to_owned(),
        kind,
    });
}

#[cfg(test)]
mod tests {
    use super::super::{parse, parse_with_notices, Expr, Filter};
    use super::*;

    fn names() -> CompletionNames {
        CompletionNames {
            streams: vec!["Work".into(), "Side project".into(), "Home".into()],
            contexts: vec!["errands".into(), "home".into()],
        }
    }

    fn labels(input: &str) -> Vec<String> {
        complete(input, &names())
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    fn texts(input: &str) -> Vec<String> {
        complete(input, &names())
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn a_bare_word_completes_to_the_keys_it_starts() {
        assert_eq!(labels("d"), ["due:", "done:"]);
        assert_eq!(texts("deploy st"), ["deploy stream:", "deploy state:"]);
        assert_eq!(complete("i", &names())[0].kind, CompletionKind::Key("is"));
    }

    #[test]
    fn a_whole_key_or_a_trailing_space_offers_nothing() {
        assert!(labels("").is_empty());
        assert!(labels("deploy ").is_empty());
        assert!(labels("is").iter().all(|l| l != "is:"));
        assert!(labels("passport").is_empty());
    }

    #[test]
    fn a_key_completes_to_its_values() {
        assert_eq!(labels("is:la"), ["late", "late-hard"]);
        assert_eq!(texts("deploy is:st"), ["deploy is:stale "]);
        assert_eq!(
            complete("has:a", &names())[0].kind,
            CompletionKind::Value {
                key: "has",
                value: "attachment"
            }
        );
        assert_eq!(
            labels("state:"),
            ["todo", "in_progress", "done", "cancelled", "open"]
        );
        assert_eq!(labels("kind:no"), ["note"]);
    }

    #[test]
    fn a_comma_list_completes_its_last_item() {
        assert_eq!(texts("kind:task,no"), ["kind:task,note "]);
        assert_eq!(texts("context:errands,h"), ["context:errands,home "]);
    }

    #[test]
    fn a_date_operator_completes_date_words_whatever_its_comparator() {
        assert_eq!(labels("due<tom"), ["tomorrow"]);
        assert_eq!(
            texts("planned>=this"),
            ["planned>=this-week ", "planned>=this-month "]
        );
        assert_eq!(
            complete("done:yes", &names())[0].kind,
            CompletionKind::Value {
                key: "date",
                value: "yesterday"
            }
        );
        // `state` does not compare, so `state<` is no operator.
        assert!(labels("state<d").is_empty());
    }

    #[test]
    fn a_shorthand_completes_names_quoting_the_ones_that_need_it() {
        assert_eq!(labels("#"), ["#Home", "#\"Side project\"", "#Work"]);
        assert_eq!(texts("deploy #si"), ["deploy #\"Side project\" "]);
        assert_eq!(texts("#\"Side pr"), ["#\"Side project\" "]);
        assert_eq!(labels("@"), ["@errands", "@home"]);
        assert_eq!(complete("@e", &names())[0].kind, CompletionKind::Context);
        // A name already whole is not offered again.
        assert!(labels("#Work").is_empty());
    }

    #[test]
    fn stream_and_context_keys_complete_names() {
        assert_eq!(labels("stream:w"), ["Work"]);
        assert_eq!(labels("stream:s"), ["\"Side project\""]);
        assert_eq!(
            complete("stream:w", &names())[0].kind,
            CompletionKind::Stream
        );
        assert_eq!(labels("context:"), ["errands", "home"]);
    }

    #[test]
    fn priority_completes_its_scale() {
        assert_eq!(labels("!"), ["!1", "!2", "!3", "!4", "!5"]);
        assert_eq!(labels("priority<="), ["1", "2", "3", "4", "5"]);
        assert!(labels("!1").is_empty());
    }

    #[test]
    fn negation_and_groups_keep_what_comes_before_the_word() {
        assert_eq!(texts("-is:bl"), ["-is:blocked "]);
        assert_eq!(texts("(@home OR @er"), ["(@home OR @errands "]);
    }

    #[test]
    fn no_more_than_the_cap_is_offered() {
        assert_eq!(labels("due:").len(), MAX_COMPLETIONS);
        let many = CompletionNames {
            streams: (0..20).map(|i| format!("s{i:02}")).collect(),
            contexts: vec![],
        };
        assert_eq!(complete("#s", &many).len(), MAX_COMPLETIONS);
    }

    /// Every value and name suggested reads back as the operator it names,
    /// never as text: the vocabulary is the parser's.
    #[test]
    fn every_suggestion_parses_as_the_operator_it_completes() {
        let inputs = [
            "is:", "has:", "state:", "kind:", "due:", "planned<", "!", "#", "@", "stream:",
            "context:",
        ];
        for input in inputs {
            for c in complete(input, &names()) {
                let parsed = parse_with_notices(&c.text);
                assert!(
                    parsed.notices.is_empty(),
                    "{:?} raised {:?}",
                    c.text,
                    parsed.notices
                );
                assert!(
                    matches!(parse(&c.text).clauses.as_slice(), [Expr::Filter(_)]),
                    "{:?} is not one filter",
                    c.text
                );
            }
        }
        let side = parse(&texts("#si")[0]);
        assert_eq!(
            side.clauses,
            [Expr::Filter(Filter::Stream(vec!["Side project".into()]))]
        );
    }

    /// A key suggestion followed by any value it suggests is one filter.
    #[test]
    fn every_key_suggested_takes_the_values_suggested_for_it() {
        for key in KEYS {
            let with_key = format!("{key}:");
            for c in complete(&with_key, &names()) {
                assert!(matches!(
                    parse(&c.text).clauses.as_slice(),
                    [Expr::Filter(_)]
                ));
            }
        }
    }

    #[test]
    fn a_scope_token_reads_back_as_the_one_filter_it_names() {
        assert_eq!(scope_token(Scope::Stream, "work"), "#work ");
        assert_eq!(scope_token(Scope::Context, "errands"), "@errands ");
        for name in ["Side project", "a,b", "(x)", "東京"] {
            let stream = parse(&scope_token(Scope::Stream, name));
            assert_eq!(
                stream.clauses,
                [Expr::Filter(Filter::Stream(vec![name.into()]))]
            );
            let context = parse(&scope_token(Scope::Context, name));
            assert_eq!(
                context.clauses,
                [Expr::Filter(Filter::Context(vec![name.into()]))]
            );
        }
    }

    #[test]
    fn hostile_input_never_panics() {
        for input in [
            "\"", "\"\"", "((", "-", "--", "#\"", "@\"\"", "is:,", "kind:,,", "!!", ":", "<=",
        ] {
            let _ = complete(input, &names());
        }
    }
}
