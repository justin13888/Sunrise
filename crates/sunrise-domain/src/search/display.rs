//! The canonical text of a query, which [`super::parse()`] reads back as the
//! same query.
//!
//! Clauses are separated by one space. A single stream, context or priority
//! is written with its shorthand (`#work`, `@home`, `!1`), every other filter
//! as `key cmp value` with lowercase keywords, and a name is quoted when it
//! holds anything a bare value cannot.

use super::ast::{Expr, Filter, Keyword, SearchQuery};
use std::fmt::{self, Write as _};

fn join(f: &mut fmt::Formatter<'_>, items: &[Expr], sep: &str) -> fmt::Result {
    for (i, e) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(sep)?;
        }
        write!(f, "{e}")?;
    }
    Ok(())
}

/// A name as one item of a comma list: bare when it can be, else quoted.
fn list_name(name: &str) -> String {
    if name
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | ','))
    {
        format!("\"{name}\"")
    } else {
        name.to_owned()
    }
}

/// A name after `#` or `@`, where a comma is part of the name.
fn shorthand_name(name: &str) -> String {
    if name
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '(' | ')'))
    {
        format!("\"{name}\"")
    } else {
        name.to_owned()
    }
}

fn list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| list_name(n))
        .collect::<Vec<_>>()
        .join(",")
}

fn words<T: Keyword>(values: &[T]) -> String {
    values
        .iter()
        .map(|v| v.keyword())
        .collect::<Vec<_>>()
        .join(",")
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(n) if n.len() == 1 => write!(f, "#{}", shorthand_name(&n[0])),
            Self::Stream(n) => write!(f, "stream:{}", list(n)),
            Self::Context(n) if n.len() == 1 => write!(f, "@{}", shorthand_name(&n[0])),
            Self::Context(n) => write!(f, "context:{}", list(n)),
            Self::Place(n) => write!(f, "place:{}", list(n)),
            Self::State(v) => write!(f, "state:{}", words(v)),
            Self::Kind(v) => write!(f, "kind:{}", words(v)),
            Self::Is(v) => write!(f, "is:{}", words(v)),
            Self::Has(v) => write!(f, "has:{}", words(v)),
            Self::Priority(cmp, v) => {
                if let ([p], super::ast::Cmp::Eq) = (v.as_slice(), cmp) {
                    return write!(f, "!{p}");
                }
                f.write_str("priority")?;
                f.write_str(cmp.as_str())?;
                let mut first = true;
                for p in v {
                    if !first {
                        f.write_char(',')?;
                    }
                    first = false;
                    write!(f, "{p}")?;
                }
                Ok(())
            }
            Self::Date(field, cmp, span) => write!(f, "{}{}{span}", field.key(), cmp.as_str()),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term(t) => f.write_str(t),
            Self::Phrase(p) => write!(f, "\"{p}\""),
            Self::Filter(x) => write!(f, "{x}"),
            Self::Not(e) => write!(f, "-{e}"),
            Self::Or(items) => join(f, items, " OR "),
            Self::Group(items) => {
                f.write_char('(')?;
                join(f, items, " ")?;
                f.write_char(')')
            }
        }
    }
}

impl fmt::Display for SearchQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        join(f, &self.clauses, " ")
    }
}
