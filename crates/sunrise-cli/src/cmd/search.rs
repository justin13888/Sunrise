//! `search [--kind <kinds>] [--all] <query>...` — the search grammar over
//! every indexed kind, printed group by group in the core's fixed order
//! (`docs/08-features/search.md` §Clients, ADR-0052).
//!
//! The parsing, the ranking and the grouping are the core's
//! (`Query::SearchGrouped`); this prints them. What the query read as text or
//! matched nothing for goes to stderr as notes, so stdout stays the hits.

use std::collections::BTreeMap;

use sunrise_cli::i18n::strings;
use sunrise_core::{Query, QueryResult, SearchNotice, SearchResults};
use sunrise_domain::search::{NoticeKind, SearchKind};
use sunrise_domain::Task;
use sunrise_id::EntityRef;

use super::{CmdResult, Ctx};

/// How `search` was asked.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    /// `--kind task,note`: the kinds to answer with, as `kind:` takes them.
    kinds: Vec<SearchKind>,
    /// `--all`: every hit of every group, not the first 50 tasks and 10 of
    /// the rest.
    all: bool,
    /// The query, its words joined by single spaces.
    text: String,
}

fn kinds_of(list: &str) -> Result<Vec<SearchKind>, String> {
    list.split(',')
        .map(|k| {
            SearchKind::from_keyword(&k.to_ascii_lowercase())
                .ok_or_else(|| strings::search::unknown_kind(k))
        })
        .collect()
}

fn parse_args(rest: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut words: Vec<&str> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                words.extend(it.by_ref().map(String::as_str));
            }
            "--all" => args.all = true,
            "--kind" => {
                let list = it.next().ok_or_else(strings::search::usage)?;
                args.kinds.extend(kinds_of(list)?);
            }
            other => match other.strip_prefix("--kind=") {
                Some(list) => args.kinds.extend(kinds_of(list)?),
                None => words.push(other),
            },
        }
    }
    args.text = words.join(" ");
    if args.text.trim().is_empty() && args.kinds.is_empty() {
        return Err(strings::search::needs_query());
    }
    Ok(args)
}

/// The query the core is asked: the words, then `--kind` as a `kind:`
/// clause, which ANDs with the rest.
fn query_text(args: &Args) -> String {
    if args.kinds.is_empty() {
        return args.text.clone();
    }
    let kinds: Vec<&str> = args.kinds.iter().map(|k| k.as_str()).collect();
    format!("{} kind:{}", args.text, kinds.join(","))
        .trim()
        .to_owned()
}

fn heading(kind: SearchKind, total: u32) -> String {
    let count = i64::from(total);
    match kind {
        SearchKind::Task => strings::search::group_task(count),
        SearchKind::Note => strings::search::group_note(count),
        SearchKind::Event => strings::search::group_event(count),
        SearchKind::Block => strings::search::group_block(count),
        SearchKind::Stream => strings::search::group_stream(count),
        SearchKind::Context => strings::search::group_context(count),
        SearchKind::Place => strings::search::group_place(count),
        SearchKind::Attachment => strings::search::group_attachment(count),
        SearchKind::View => strings::search::group_view(count),
        SearchKind::Person => strings::search::group_person(count),
    }
}

/// A notice, worded for the person: what part of the query, and why.
fn notice(text: &str, n: &SearchNotice) -> String {
    let at: String = text
        .chars()
        .skip(n.start as usize)
        .take(n.end.saturating_sub(n.start) as usize)
        .collect();
    let what = n.detail.clone().unwrap_or(at);
    match n.kind {
        NoticeKind::UnknownKey => strings::search::notice_unknown_key(&what),
        NoticeKind::BadValue => strings::search::notice_bad_value(&what),
        NoticeKind::UnclosedQuote => strings::search::notice_unclosed_quote(),
        NoticeKind::UnclosedGroup => strings::search::notice_unclosed_group(),
        NoticeKind::StrayParen => strings::search::notice_stray_paren(),
        NoticeKind::EmptyPhrase => strings::search::notice_empty_phrase(),
        NoticeKind::LiteralOr => strings::search::notice_literal_or(),
        NoticeKind::TooLong => strings::search::notice_too_long(),
        NoticeKind::TooManyClauses => strings::search::notice_too_many_clauses(),
        NoticeKind::TooDeep => strings::search::notice_too_deep(),
        NoticeKind::UnknownName => strings::search::notice_unknown_name(&what),
        NoticeKind::AmbiguousName => strings::search::notice_ambiguous_name(&what),
        NoticeKind::Unsupported => strings::search::notice_unsupported(&what),
    }
}

/// The tasks among the hits, read whole for `--json`.
async fn hit_tasks(ctx: &Ctx<'_>, r: &SearchResults) -> CmdResult<BTreeMap<EntityRef, Task>> {
    let mut out = BTreeMap::new();
    for g in r.groups.iter().filter(|g| g.kind == SearchKind::Task) {
        for h in &g.hits {
            if let QueryResult::Task(t) = ctx.core.query(Query::EntityById(h.entity)).await? {
                out.insert(h.entity, *t);
            }
        }
    }
    Ok(out)
}

/// `search [--kind <kinds>] [--all] <query>...`.
pub(super) async fn search(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = parse_args(rest)?;
    // Cheap when the word index already suits the content language; when it
    // does not, the re-index runs here rather than leaving a CLI-only user on
    // the old tokenizer forever.
    ctx.core.reindex_search().await?;
    let text = query_text(&args);
    let q = Query::SearchGrouped {
        text: text.clone(),
        now_ms: ctx.core.now_ms(),
        prefix: false,
        per_group: args.all.then_some(u32::MAX),
    };
    let QueryResult::Search(r) = ctx.core.query(q).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    for n in &r.notices {
        eprintln!("{}", strings::general::note(&notice(&text, n)));
    }
    if ctx.json {
        let tasks = hit_tasks(ctx, &r).await?;
        sunrise_cli::json::print(&sunrise_cli::json::search(&r, &tasks, &ctx.reader()));
        return Ok(());
    }
    if r.groups.is_empty() {
        eprintln!("{}", strings::search::nothing(&text));
        return Ok(());
    }
    for g in &r.groups {
        println!("{}", heading(g.kind, g.total));
        for h in &g.hits {
            println!("  {}  {}", h.entity.to_str(), h.title.text);
            if let Some(s) = &h.snippet {
                println!("      {}", s.text);
            }
        }
        let shown = u32::try_from(g.hits.len()).unwrap_or(u32::MAX);
        if g.total > shown {
            println!(
                "  {}",
                strings::search::more(i64::from(g.total - shown), g.kind.as_str())
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn kinds_and_all_are_flags_and_the_rest_is_the_query() {
        let a = parse_args(&v(&["--kind", "task,note", "#work", "--all", "deploy"])).unwrap();
        assert_eq!(a.kinds, [SearchKind::Task, SearchKind::Note]);
        assert!(a.all);
        assert_eq!(query_text(&a), "#work deploy kind:task,note");
        let a = parse_args(&v(&["--kind=stream"])).unwrap();
        assert_eq!(query_text(&a), "kind:stream");
        let a = parse_args(&v(&["--", "--kind", "x"])).unwrap();
        assert_eq!(query_text(&a), "--kind x");
    }

    #[test]
    fn a_query_or_a_kind_is_required_and_kinds_are_checked() {
        assert!(parse_args(&v(&[])).is_err());
        assert!(parse_args(&v(&["  "])).is_err());
        assert!(parse_args(&v(&["--kind", "spaceship", "x"])).is_err());
        assert!(parse_args(&v(&["x", "--kind"])).is_err());
    }
}
