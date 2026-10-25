//! Search: running a parsed query over the two full-text indexes and the
//! entity rows, and keeping the indexes built for the vault's content
//! language (ADR-0052, `docs/08-features/search.md`).
//!
//! # Running a query
//!
//! [`sunrise_domain::search::parse`] reads the text. Every term and phrase
//! becomes a set of docs, from both indexes: the word index (`search_words`,
//! ranked by `bm25` with column weights 10, 5 and 1) and the trigram index
//! (`search_tri`, which finds CJK text and substrings over folded text). A
//! term of one or two characters that holds CJK cannot use trigrams, and
//! scans titles with `LIKE` instead, capped at [`SHORT_CJK_CAP`] rows. Only
//! quoted text reaches FTS5, so no query is an FTS5 syntax error.
//!
//! Structured operators are evaluated in Rust over the candidate entities'
//! rows, after names resolve (exact, then unique prefix, as capture does)
//! and dates resolve in the reader's zone and week. A date is compared as
//! the civil day the reader sees it on ([`SunriseTime::day_in`]), never
//! through the UTC-as-civil storage index (`docs/10-cross-cutting/time.md`).
//!
//! Hits are grouped by kind in the fixed order, and within a group ranked:
//! by how many terms the word index matched, then its `bm25`, then the
//! trigram index's `bm25`, so word hits come before trigram-only ones; a
//! query with no text lists the newest first. Highlights are computed here.
//!
//! # Keeping the indexes built
//!
//! `search_meta` records the layout version ([`SEARCH_INDEX_V`]) and whether
//! the word index stems. [`Engine::ensure_search_index`], at open, writes
//! both indexes from the entity rows when that record is missing or names
//! another layout: the first open after migration 0040. A change of the
//! content language (`search.content_language`) changes only whether the
//! word index stems; [`Engine::reindex_search_step`] re-tokenizes it into a
//! shadow table a batch of [`REINDEX_BATCH`] docs at a time, each its own
//! transaction, and swaps it in with the last. Search answers from the old
//! index until then, a write to a doc already copied is made to both, and a
//! re-index that stops part way resumes from its cursor.

use super::attachment::read_attachment;
use super::block::{block_row, read_block};
use super::ids::{blob16, time_from_parts};
use super::merge::{create_words_sql, rebuild_search_from_rows};
use super::snapshot::in_tx;
use super::task::read_task;
use super::{Engine, EngineError};
use crate::queries::{QueryResult, SearchGroup, SearchHit, SearchNotice, SearchResults};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};
use sunrise_domain::search::{
    self, fold, is_cjk, match_ranges, snippet, Cmp, DateField, Days, Expr, Filter, HasValue,
    Highlighted, IsValue, NoticeKind, SearchKind, SearchQuery, StateValue,
};
use sunrise_domain::{
    inbox_stream_ref, resolve_named, NameKind, NamedRef, PrefValue, SunriseTime,
    PRIVATE_DOMAIN_BYTES,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The layout of the full-text indexes this build writes. A vault whose
/// `search_meta` names another is rebuilt from its rows at open.
pub(crate) const SEARCH_INDEX_V: i64 = 1;

/// Docs a re-index copies per transaction (`search.md` §Content language).
pub(crate) const REINDEX_BATCH: i64 = 500;

/// The most rows a one- or two-character CJK term's `LIKE` scan returns.
pub(crate) const SHORT_CJK_CAP: i64 = 500;

/// Hits per group, by default: 50 tasks and 10 of every other kind.
const TASK_GROUP: u32 = 50;
const OTHER_GROUP: u32 = 10;

/// Past this many candidates of a kind, their rows are read in one scan of
/// the kind's table rather than one lookup each.
const SCAN_ABOVE: usize = 2_000;

/// The column weights of `bm25` over `search_words`: title and name, then
/// context names, then body.
const BM25: &str = "bm25(search_words, 10.0, 5.0, 1.0)";

/// What one step of a re-index did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchReindex {
    /// Docs copied into the new word index so far.
    pub copied: u64,
    /// Docs in the index.
    pub total: u64,
    /// Whether the new index is in place, or nothing needed doing.
    pub finished: bool,
}

/// How `search_meta` says the indexes were built.
struct Meta {
    index_v: i64,
    stemmed: bool,
    reindex_stemmed: Option<bool>,
    reindex_cursor: i64,
}

fn read_meta(conn: &Connection) -> rusqlite::Result<Option<Meta>> {
    conn.query_row(
        "SELECT index_v, stemmed, reindex_stemmed, reindex_cursor FROM search_meta WHERE id = 1",
        [],
        |r| {
            Ok(Meta {
                index_v: r.get(0)?,
                stemmed: r.get(1)?,
                reindex_stemmed: r.get(2)?,
                reindex_cursor: r.get(3)?,
            })
        },
    )
    .optional()
}

/// Whether a content language is English, and so stems: its primary subtag
/// is `en`. No language set reads as English, which is what every vault's
/// index did before the language was a choice.
fn stems(language: Option<&str>) -> bool {
    language.is_none_or(|tag| {
        tag.split(['-', '_'])
            .next()
            .is_some_and(|primary| primary.eq_ignore_ascii_case("en"))
    })
}

/// How a word index tokenizes, as its log events name it.
fn mode_of(stemmed: bool) -> &'static str {
    if stemmed {
        "stemmed"
    } else {
        "unstemmed"
    }
}

impl Engine {
    /// The vault's content language, and whether its word index should stem.
    fn content_language(&self, db: &Db) -> Result<(Option<String>, bool), EngineError> {
        let language = match self.resolved_preference(db, "search.content_language")? {
            Some(PrefValue::Text(tag)) => Some(tag),
            _ => None,
        };
        let stemmed = stems(language.as_deref());
        Ok((language, stemmed))
    }

    /// Write both indexes from the entity rows when `search_meta` has no
    /// record of them or names another layout than this build's: the first
    /// open after migration 0040. In one transaction. Returns how many docs
    /// it wrote, or `None` when the indexes were current.
    ///
    /// # Errors
    /// Storage failures, after which the indexes are as they were.
    pub(crate) fn ensure_search_index(&self, db: &mut Db) -> Result<Option<u64>, EngineError> {
        if read_meta(db.conn())?.is_some_and(|m| m.index_v == SEARCH_INDEX_V) {
            return Ok(None);
        }
        let (language, stemmed) = self.content_language(db)?;
        let n = in_tx(db, |tx| {
            let n = rebuild_search_from_rows(tx, stemmed)?;
            tx.execute(
                "INSERT INTO search_meta (id, index_v, stemmed, language, reindex_stemmed, reindex_cursor)
                 VALUES (1, ?1, ?2, ?3, NULL, 0)
                 ON CONFLICT (id) DO UPDATE SET
                    index_v = excluded.index_v, stemmed = excluded.stemmed,
                    language = excluded.language, reindex_stemmed = NULL, reindex_cursor = 0",
                params![SEARCH_INDEX_V, stemmed, language],
            )?;
            Ok(n)
        })?;
        tracing::info!(
            ev = "core.search.index_built",
            n_docs = n,
            mode = mode_of(stemmed),
            "the search indexes were written from the entity rows"
        );
        Ok(Some(n))
    }

    /// One step of re-tokenizing the word index for the content language:
    /// start the shadow index, copy the next [`REINDEX_BATCH`] docs into it,
    /// or swap it in. One transaction. A step when the index already suits
    /// the language does nothing and reports `finished`.
    ///
    /// # Errors
    /// Storage failures, after which the step is as if it never ran.
    pub(crate) fn reindex_search_step(&self, db: &mut Db) -> Result<SearchReindex, EngineError> {
        self.reindex_search_step_by(db, REINDEX_BATCH)
    }

    /// [`Self::reindex_search_step`], copying `batch` docs a step.
    pub(super) fn reindex_search_step_by(
        &self,
        db: &mut Db,
        batch: i64,
    ) -> Result<SearchReindex, EngineError> {
        let (language, want) = self.content_language(db)?;
        let Some(meta) = read_meta(db.conn())? else {
            // Never built: the open builds it whole.
            self.ensure_search_index(db)?;
            return Ok(SearchReindex {
                finished: true,
                ..SearchReindex::default()
            });
        };
        if meta.reindex_stemmed.is_none() && meta.stemmed == want {
            let total: i64 = db
                .conn()
                .query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get(0))?;
            return Ok(SearchReindex {
                copied: total.unsigned_abs(),
                total: total.unsigned_abs(),
                finished: true,
            });
        }
        in_tx(db, |tx| {
            let mut cursor = meta.reindex_cursor;
            if meta.reindex_stemmed != Some(want) {
                // Starting, or the language changed again part way.
                tx.execute_batch("DROP TABLE IF EXISTS search_words_next")?;
                tx.execute_batch(&create_words_sql("search_words_next", want))?;
                cursor = 0;
                tracing::info!(
                    ev = "core.search.reindex_started",
                    mode = mode_of(want),
                    "the word index is being rebuilt for the content language"
                );
            }
            let copied = tx.execute(
                "INSERT INTO search_words_next (rowid, title, aux, body)
                 SELECT rowid, title, aux, body FROM search_words
                 WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
                params![cursor, batch],
            )?;
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(rowid), ?1) FROM search_words_next",
                [cursor],
                |r| r.get(0),
            )?;
            let total: i64 = tx.query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get(0))?;
            let done: i64 = tx.query_row(
                "SELECT COUNT(*) FROM search_docs WHERE doc <= ?1",
                [next],
                |r| r.get(0),
            )?;
            let finished = i64::try_from(copied).unwrap_or(i64::MAX) < batch;
            if finished {
                tx.execute_batch(
                    "DROP TABLE search_words;
                     ALTER TABLE search_words_next RENAME TO search_words;",
                )?;
                tx.execute(
                    "UPDATE search_meta SET stemmed = ?1, language = ?2,
                        reindex_stemmed = NULL, reindex_cursor = 0 WHERE id = 1",
                    params![want, language],
                )?;
                tracing::info!(
                    ev = "core.search.reindex_done",
                    n_docs = total,
                    mode = mode_of(want),
                    "the word index now follows the content language"
                );
            } else {
                tx.execute(
                    "UPDATE search_meta SET reindex_stemmed = ?1, reindex_cursor = ?2 WHERE id = 1",
                    params![want, next],
                )?;
            }
            Ok(SearchReindex {
                copied: done.unsigned_abs(),
                total: total.unsigned_abs(),
                finished,
            })
        })
    }

    /// [`crate::Query::Search`]: the Tasks group of
    /// [`crate::Query::SearchGrouped`], as tasks.
    pub(super) fn query_search(
        &self,
        db: &Db,
        text: &str,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        let parsed = search::parse(text);
        let ids = Run::new(self, db, &parsed, self.clock.now_ms(), false)?
            .ranked(Some(SearchKind::Task))?;
        let mut tasks = Vec::new();
        for (_, hit) in ids.into_iter().take(limit as usize) {
            if let Some(t) = read_task(db.conn(), &hit.id)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::Tasks(tasks))
    }

    /// [`crate::Query::SearchGrouped`].
    pub(super) fn query_search_grouped(
        &self,
        db: &Db,
        text: &str,
        now_ms: u64,
        prefix: bool,
        per_group: Option<u32>,
    ) -> Result<QueryResult, EngineError> {
        let parsed = search::parse_with_notices(text);
        let mut notices: Vec<SearchNotice> = parsed
            .notices
            .iter()
            .map(|n| SearchNotice {
                kind: n.kind,
                start: n.start,
                end: n.end,
                detail: None,
            })
            .collect();
        let mut run = Run::new(self, db, &parsed.query, now_ms, prefix)?;
        let ranked = run.ranked(None)?;
        notices.append(&mut run.notices(text));
        notices.sort_by_key(|n| (n.start, n.end));
        let needles = parsed.query.positive_text();
        let mut by_kind: BTreeMap<SearchKind, Vec<Candidate>> = BTreeMap::new();
        for (kind, c) in ranked {
            by_kind.entry(kind).or_default().push(c);
        }
        let mut groups = Vec::new();
        for (kind, hits) in by_kind {
            let limit = per_group.unwrap_or(if kind == SearchKind::Task {
                TASK_GROUP
            } else {
                OTHER_GROUP
            });
            let total = u32::try_from(hits.len()).unwrap_or(u32::MAX);
            let mut out = Vec::new();
            for c in hits.into_iter().take(limit as usize) {
                if let Some(hit) = hit_of(db.conn(), kind, &c, &needles)? {
                    out.push(hit);
                }
            }
            groups.push(SearchGroup {
                kind,
                hits: out,
                total,
            });
        }
        Ok(QueryResult::Search(Box::new(SearchResults {
            groups,
            notices,
            canonical: parsed.query.to_string(),
        })))
    }
}

/// The entity kind a search kind's docs are.
fn entity_kind(kind: SearchKind) -> Option<EntityKind> {
    Some(match kind {
        SearchKind::Task => EntityKind::Task,
        SearchKind::Stream => EntityKind::Stream,
        SearchKind::Context => EntityKind::Context,
        SearchKind::Block => EntityKind::Block,
        SearchKind::Attachment => EntityKind::Attachment,
        SearchKind::View => EntityKind::SavedView,
        _ => return None,
    })
}

/// One indexed entity that may match.
#[derive(Debug, Clone)]
struct Candidate {
    doc: i64,
    id: [u8; 16],
    /// Positive text atoms the word index matched it for, and their summed
    /// `bm25` (lower is better).
    word_hits: u32,
    score: f64,
    /// The summed `bm25` of the trigram index, which orders the hits only
    /// it found.
    tri_score: f64,
    created_ms: i64,
}

/// The rows of an entity a filter reads.
#[derive(Debug, Clone, Default)]
struct Attrs {
    /// The task's or block's stream, the private domain's id for none: the
    /// id the `Inbox` name of `stream:` resolves to.
    stream: Option<[u8; 16]>,
    /// When the task was triaged out of the Inbox (ADR-0046 §3), in ms.
    triaged_at_ms: Option<i64>,
    state: Option<String>,
    priority: Option<u8>,
    estimate: bool,
    planned: Option<SunriseTime>,
    hard: Option<SunriseTime>,
    done: Option<SunriseTime>,
    created_ms: i64,
    recurring: bool,
    has_note: bool,
}

/// A stored `stream_id` column: the stream, or the private domain's id for a
/// task or block that has none.
fn stream_or_none(raw: Option<Vec<u8>>) -> [u8; 16] {
    raw.map_or(PRIVATE_DOMAIN_BYTES, |b| blob16(&b))
}

/// A query, compiled against this vault.
enum Node {
    Text(usize),
    Kinds(Vec<SearchKind>),
    Streams(HashSet<[u8; 16]>),
    Contexts(HashSet<[u8; 16]>),
    State(Vec<StateValue>),
    Priority(Cmp, Vec<u8>),
    Date(DateField, Cmp, Option<Days>),
    Is(Vec<IsValue>),
    Has(Vec<HasValue>),
    Never,
    Not(Box<Node>),
    Or(Vec<Node>),
    And(Vec<Node>),
}

/// One term or phrase, and the docs it matches.
struct Atom {
    text: String,
    prefix: bool,
    /// Whether it ranks hits: it is not under a negation. A negated atom is
    /// only a membership test, so no `bm25` is computed for it.
    ranked: bool,
    /// Docs the word index matched, with their `bm25`.
    words: HashMap<i64, f64>,
    /// Docs the trigram index matched, with its `bm25`; a title scan's hits
    /// have none.
    tri: HashMap<i64, f64>,
    /// Docs either index matched.
    all: HashSet<i64>,
}

/// A notice raised while compiling, before its span is found.
struct Pending {
    kind: NoticeKind,
    detail: String,
}

/// One query run.
struct Run<'a> {
    db: &'a Db,
    root: Node,
    atoms: Vec<Atom>,
    pending: Vec<Pending>,
    tz: TimeZone,
    now_ms: u64,
    attrs: HashMap<[u8; 16], Attrs>,
    contexts: HashMap<[u8; 16], HashSet<[u8; 16]>>,
    flags: HashMap<([u8; 16], u8), bool>,
    /// The `blocked_by` edges the read-time cycle rule sets aside, as the
    /// key list [`super::task::suppressed_edge_keys`] writes, read once on
    /// the first `is:blocked` or `has:dependency` (ADR-0065 clarification
    /// D18): a set-aside edge neither blocks nor is a dependency here, as it
    /// is in every other read of the graph.
    suppressed: Option<String>,
}

/// One stream or context a name can resolve to: its id, name and parent.
type Named = (EntityRef, String, Option<[u8; 16]>);

/// The names of streams or contexts, and each stream's parent.
fn names(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Named>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    rows.map(|r| {
        r.map(|(id, name, parent)| {
            (
                EntityRef::new(EntityKind::Stream, blob16(&id)),
                name,
                parent.map(|p| blob16(&p)),
            )
        })
    })
    .collect()
}

impl<'a> Run<'a> {
    fn new(
        engine: &Engine,
        db: &'a Db,
        query: &SearchQuery,
        now_ms: u64,
        prefix: bool,
    ) -> Result<Self, EngineError> {
        let tz = engine.device_zone();
        let today = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).unwrap_or(i64::MAX))
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .to_zoned(tz.clone())
            .date();
        let week_start = engine.week_start(db)?;
        let mut streams = names(
            db.conn(),
            "SELECT stream_id, name, parent_id FROM streams
             WHERE deleted = 0 AND name IS NOT NULL AND name <> ''",
        )?;
        // A stream the parent cycle rule reads as a root nests under nothing
        // (`stream::rooted_streams`), so `stream:` takes no loop round.
        let rooted = super::stream::rooted_streams(db.conn())?;
        for (id, _, parent) in &mut streams {
            if rooted.iter().any(|r| r.stream == *id) {
                *parent = None;
            }
        }
        // `stream:Inbox` names the tasks of no stream, as it named the
        // Inbox sentinel's before ADR-0046: the private domain's id, which a
        // stream-less task's `stream` attribute holds.
        let inbox = inbox_stream_ref();
        if !streams.iter().any(|(id, _, _)| *id == inbox) {
            streams.push((inbox, "Inbox".to_owned(), None));
        }
        let contexts = names(
            db.conn(),
            "SELECT id, name, NULL FROM contexts WHERE deleted = 0",
        )?
        .into_iter()
        .map(|(id, n, p)| (EntityRef::new(EntityKind::Context, *id.bytes()), n, p))
        .collect::<Vec<_>>();
        let last = if prefix { query.last_term() } else { None };
        let mut c = Compiler {
            streams: &streams,
            contexts: &contexts,
            today,
            week_start,
            atoms: Vec::new(),
            pending: Vec::new(),
            last,
            negated: false,
        };
        let root = Node::And(query.clauses.iter().map(|e| c.node(e)).collect());
        let (mut atoms, pending) = (c.atoms, c.pending);
        for atom in &mut atoms {
            match_atom(db.conn(), atom)?;
        }
        Ok(Self {
            db,
            root,
            atoms,
            pending,
            tz,
            now_ms,
            attrs: HashMap::new(),
            contexts: HashMap::new(),
            flags: HashMap::new(),
            suppressed: None,
        })
    }

    /// The compile-time notices, each placed on the first place its detail
    /// appears in `text`.
    fn notices(&self, text: &str) -> Vec<SearchNotice> {
        let lower: Vec<char> = text.to_lowercase().chars().collect();
        self.pending
            .iter()
            .map(|p| {
                let needle: Vec<char> = p.detail.to_lowercase().chars().collect();
                let at = (!needle.is_empty() && needle.len() <= lower.len())
                    .then(|| {
                        (0..=lower.len() - needle.len())
                            .find(|&i| lower[i..i + needle.len()] == needle[..])
                    })
                    .flatten();
                let (start, end) = at.map_or((0, 0), |i| (i, i + needle.len()));
                SearchNotice {
                    kind: p.kind,
                    start: u32::try_from(start).unwrap_or(u32::MAX),
                    end: u32::try_from(end).unwrap_or(u32::MAX),
                    detail: Some(p.detail.clone()),
                }
            })
            .collect()
    }

    /// The docs a hit must be among, from the clauses every hit matches:
    /// a positive term, or an OR of them. `None` when no clause narrows.
    fn required(&self) -> Option<HashSet<i64>> {
        let Node::And(items) = &self.root else {
            return None;
        };
        let mut out: Option<HashSet<i64>> = None;
        for item in items {
            let set = match item {
                Node::Text(i) => self.atoms[*i].all.clone(),
                Node::Or(branches) if branches.iter().all(|b| matches!(b, Node::Text(_))) => {
                    let mut u = HashSet::new();
                    for b in branches {
                        if let Node::Text(i) = b {
                            u.extend(self.atoms[*i].all.iter().copied());
                        }
                    }
                    u
                }
                _ => continue,
            };
            out = Some(match out {
                Some(prev) => prev.intersection(&set).copied().collect(),
                None => set,
            });
        }
        out
    }

    /// Every hit, ranked within its kind, of `only` when it is set.
    fn ranked(
        &mut self,
        only: Option<SearchKind>,
    ) -> Result<Vec<(SearchKind, Candidate)>, EngineError> {
        if matches!(&self.root, Node::And(items) if items.is_empty()) {
            return Ok(Vec::new());
        }
        let docs = self.candidates(only)?;
        let mut by_kind: BTreeMap<SearchKind, Vec<Candidate>> = BTreeMap::new();
        for (kind, c) in docs {
            by_kind.entry(kind).or_default().push(c);
        }
        let mut out = Vec::new();
        let has_text = !self.atoms.is_empty();
        // Taken out while it is walked, so the walk may fill the caches.
        let root = std::mem::replace(&mut self.root, Node::Never);
        for (kind, cands) in by_kind {
            if !has_text || reads_rows(&root) {
                self.load_attrs(kind, &cands)?;
            }
            let mut hits = Vec::new();
            for mut c in cands {
                if self.eval_node(&root, kind, &c)? {
                    c.created_ms = self.attrs.get(&c.id).map_or(0, |a| a.created_ms);
                    hits.push(c);
                }
            }
            hits.sort_by(|a, b| {
                let words = b
                    .word_hits
                    .cmp(&a.word_hits)
                    .then_with(|| a.score.total_cmp(&b.score))
                    .then_with(|| a.tri_score.total_cmp(&b.tri_score));
                let order = if has_text {
                    words
                } else {
                    b.created_ms.cmp(&a.created_ms)
                };
                order.then_with(|| a.id.cmp(&b.id))
            });
            out.extend(hits.into_iter().map(|c| (kind, c)));
        }
        self.root = root;
        Ok(out)
    }

    fn candidates(
        &self,
        only: Option<SearchKind>,
    ) -> Result<Vec<(SearchKind, Candidate)>, EngineError> {
        let conn = self.db.conn();
        let make = |doc: i64, kind: String, id: Vec<u8>| {
            SearchKind::from_keyword(&kind).map(|k| {
                let mut c = Candidate {
                    doc,
                    id: blob16(&id),
                    word_hits: 0,
                    score: 0.0,
                    tri_score: 0.0,
                    created_ms: 0,
                };
                for atom in self.atoms.iter().filter(|a| a.ranked) {
                    if let Some(s) = atom.words.get(&doc) {
                        c.word_hits += 1;
                        c.score += s;
                    }
                    c.tri_score += atom.tri.get(&doc).copied().unwrap_or(0.0);
                }
                (k, c)
            })
        };
        let mut out = Vec::new();
        let required = self.required();
        if let Some(set) = required.as_ref().filter(|s| s.len() <= SCAN_ABOVE) {
            let mut docs: Vec<i64> = set.iter().copied().collect();
            docs.sort_unstable();
            let mut stmt = conn.prepare("SELECT kind, id FROM search_docs WHERE doc = ?")?;
            for doc in docs {
                if let Some((kind, id)) = stmt
                    .query_row([doc], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })
                    .optional()?
                {
                    out.extend(make(doc, kind, id));
                }
            }
        } else {
            let mut stmt = conn.prepare("SELECT doc, kind, id FROM search_docs ORDER BY doc")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for r in rows {
                let (doc, kind, id) = r?;
                if required.as_ref().is_some_and(|set| !set.contains(&doc)) {
                    continue;
                }
                out.extend(make(doc, kind, id));
            }
        }
        if let Some(k) = only {
            out.retain(|(kind, _)| *kind == k);
        }
        Ok(out)
    }

    /// Read the rows the filters of this query read, for `cands` of `kind`.
    fn load_attrs(&mut self, kind: SearchKind, cands: &[Candidate]) -> Result<(), EngineError> {
        let conn = self.db.conn();
        let (table, key, cols) = match kind {
            SearchKind::Task => (
                "tasks",
                "id",
                "stream_id, state, priority, estimated_min,
                 scheduled_at_ms, scheduled_at_kind, scheduled_at_tz, scheduled_at_civil,
                 due_at_ms, due_at_kind, due_at_tz, due_at_civil,
                 completed_at_ms, completed_at_kind, completed_at_tz, completed_at_civil,
                 created_at_ms, routine_id IS NOT NULL, length(COALESCE(body, X'')) > 0,
                 triaged_at_ms",
            ),
            SearchKind::Block => ("blocks", "id", "stream_id, created_at_ms"),
            SearchKind::Stream => (
                "streams",
                "stream_id",
                "created_at_ms, length(COALESCE(description, X'')) > 0",
            ),
            SearchKind::Context => ("contexts", "id", "created_at_ms"),
            SearchKind::Attachment => ("attachments", "id", "created_at_ms"),
            SearchKind::View => ("saved_views", "id", "created_at_ms"),
            _ => return Ok(()),
        };
        let read = |r: &rusqlite::Row<'_>, from: usize| -> rusqlite::Result<Attrs> {
            let time = |at: usize| -> rusqlite::Result<Option<SunriseTime>> {
                Ok(time_from_parts(
                    r.get(at)?,
                    r.get::<_, Option<String>>(at + 1)?.as_deref(),
                    r.get::<_, Option<String>>(at + 2)?.as_deref(),
                    r.get::<_, Option<String>>(at + 3)?.as_deref(),
                ))
            };
            Ok(match kind {
                SearchKind::Task => Attrs {
                    stream: Some(stream_or_none(r.get(from)?)),
                    triaged_at_ms: r.get(from + 19)?,
                    state: Some(r.get(from + 1)?),
                    priority: r
                        .get::<_, Option<i64>>(from + 2)?
                        .and_then(|p| u8::try_from(p).ok()),
                    estimate: r.get::<_, Option<i64>>(from + 3)?.is_some(),
                    planned: time(from + 4)?,
                    hard: time(from + 8)?,
                    done: time(from + 12)?,
                    created_ms: r.get(from + 16)?,
                    recurring: r.get(from + 17)?,
                    has_note: r.get(from + 18)?,
                },
                SearchKind::Block => Attrs {
                    stream: Some(stream_or_none(r.get(from)?)),
                    created_ms: r.get(from + 1)?,
                    ..Attrs::default()
                },
                SearchKind::Stream => Attrs {
                    created_ms: r.get(from)?,
                    has_note: r.get(from + 1)?,
                    ..Attrs::default()
                },
                _ => Attrs {
                    created_ms: r.get(from)?,
                    ..Attrs::default()
                },
            })
        };
        if cands.len() > SCAN_ABOVE {
            let wanted: HashSet<[u8; 16]> = cands.iter().map(|c| c.id).collect();
            let mut stmt = conn.prepare(&format!("SELECT {key}, {cols} FROM {table}"))?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let id = blob16(&r.get::<_, Vec<u8>>(0)?);
                if wanted.contains(&id) {
                    self.attrs.insert(id, read(r, 1)?);
                }
            }
        } else {
            let mut stmt = conn.prepare(&format!("SELECT {cols} FROM {table} WHERE {key} = ?"))?;
            for c in cands {
                if let Some(a) = stmt.query_row([&c.id[..]], |r| read(r, 0)).optional()? {
                    self.attrs.insert(c.id, a);
                }
            }
        }
        Ok(())
    }

    fn task_contexts(&mut self, id: &[u8; 16]) -> rusqlite::Result<&HashSet<[u8; 16]>> {
        if !self.contexts.contains_key(id) {
            let mut stmt = self
                .db
                .conn()
                .prepare("SELECT context_id FROM task_contexts WHERE task_id = ?")?;
            let set = stmt
                .query_map([&id[..]], |r| r.get::<_, Vec<u8>>(0))?
                .map(|r| r.map(|raw| blob16(&raw)))
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            self.contexts.insert(*id, set);
        }
        Ok(&self.contexts[id])
    }

    /// A yes-or-no fact about a task read from another table, cached.
    fn flag(&mut self, id: &[u8; 16], which: u8, sql: &str) -> rusqlite::Result<bool> {
        if let Some(v) = self.flags.get(&(*id, which)) {
            return Ok(*v);
        }
        let v: bool = self.db.conn().query_row(sql, [&id[..]], |r| r.get(0))?;
        self.flags.insert((*id, which), v);
        Ok(v)
    }

    /// [`Self::flag`] of a query over `task_blockers` that reads the
    /// set-aside edges as `?2`.
    fn edge_flag(&mut self, id: &[u8; 16], which: u8, sql: &str) -> rusqlite::Result<bool> {
        if let Some(v) = self.flags.get(&(*id, which)) {
            return Ok(*v);
        }
        if self.suppressed.is_none() {
            let keys = super::task::suppressed_edge_keys(self.db.conn())
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            self.suppressed = Some(keys);
        }
        let keys = self.suppressed.as_deref().unwrap_or_default();
        let v: bool = self
            .db
            .conn()
            .query_row(sql, rusqlite::params![&id[..], keys], |r| r.get(0))?;
        self.flags.insert((*id, which), v);
        Ok(v)
    }

    fn eval_node(
        &mut self,
        node: &Node,
        kind: SearchKind,
        c: &Candidate,
    ) -> Result<bool, EngineError> {
        let task = kind == SearchKind::Task;
        Ok(match node {
            Node::Text(i) => self.atoms[*i].all.contains(&c.doc),
            Node::Kinds(ks) => ks.contains(&kind),
            Node::Never => false,
            Node::Not(inner) => !self.eval_node(inner, kind, c)?,
            Node::Or(items) => {
                for i in items {
                    if self.eval_node(i, kind, c)? {
                        return Ok(true);
                    }
                }
                false
            }
            Node::And(items) => {
                for i in items {
                    if !self.eval_node(i, kind, c)? {
                        return Ok(false);
                    }
                }
                true
            }
            Node::Streams(set) => {
                matches!(kind, SearchKind::Task | SearchKind::Block)
                    && self
                        .attrs
                        .get(&c.id)
                        .and_then(|a| a.stream)
                        .is_some_and(|s| set.contains(&s))
            }
            Node::Contexts(set) => task && !self.task_contexts(&c.id)?.is_disjoint(set),
            Node::State(values) => {
                task && self
                    .attrs
                    .get(&c.id)
                    .and_then(|a| a.state.as_deref())
                    .is_some_and(|s| values.iter().any(|v| state_matches(*v, s)))
            }
            Node::Priority(cmp, values) => {
                task && self
                    .attrs
                    .get(&c.id)
                    .and_then(|a| a.priority)
                    .is_some_and(|p| priority_matches(*cmp, values, p))
            }
            Node::Date(field, cmp, days) => {
                let Some(days) = days else { return Ok(false) };
                let Some(a) = self.attrs.get(&c.id) else {
                    return Ok(false);
                };
                let day = match field {
                    DateField::Created => jiff::Timestamp::from_millisecond(a.created_ms)
                        .ok()
                        .map(|t| t.to_zoned(self.tz.clone()).date()),
                    DateField::Planned if task => {
                        a.planned.as_ref().and_then(|t| t.day_in(&self.tz))
                    }
                    DateField::Hard | DateField::Due if task => {
                        a.hard.as_ref().and_then(|t| t.day_in(&self.tz))
                    }
                    DateField::Done if task => a.done.as_ref().and_then(|t| t.day_in(&self.tz)),
                    _ => None,
                };
                day.is_some_and(|d| date_matches(*cmp, *days, d))
            }
            Node::Is(values) => {
                if !task {
                    return Ok(false);
                }
                let mut any = false;
                for v in values {
                    if self.is(*v, c)? {
                        any = true;
                        break;
                    }
                }
                any
            }
            Node::Has(values) => {
                let mut any = false;
                for v in values {
                    if self.has(*v, kind, c)? {
                        any = true;
                        break;
                    }
                }
                any
            }
        })
    }

    fn open(&self, id: &[u8; 16]) -> bool {
        self.attrs
            .get(id)
            .and_then(|a| a.state.as_deref())
            .is_some_and(|s| !matches!(s, "done" | "cancelled"))
    }

    fn is(&mut self, v: IsValue, c: &Candidate) -> rusqlite::Result<bool> {
        let open = self.open(&c.id);
        let Some(a) = self.attrs.get(&c.id) else {
            return Ok(false);
        };
        Ok(match v {
            IsValue::Late | IsValue::LateHard => {
                open && a
                    .hard
                    .as_ref()
                    .is_some_and(|due| sunrise_domain::is_overdue(due, self.now_ms, &self.tz))
            }
            IsValue::Recurring => a.recurring,
            IsValue::Unplanned => open && a.planned.is_none(),
            // The Inbox view (ADR-0046 §3): open, no stream, no plan, no
            // deadline, not triaged.
            IsValue::Inbox => {
                open && a.stream == Some(PRIVATE_DOMAIN_BYTES)
                    && a.planned.is_none()
                    && a.hard.is_none()
                    && a.triaged_at_ms.is_none()
            }
            IsValue::Blocked => {
                open && self.edge_flag(
                    &c.id,
                    0,
                    "SELECT EXISTS (SELECT 1 FROM task_blockers tb
                       LEFT JOIN tasks b ON b.id = tb.blocker_id
                      WHERE tb.task_id = ?1
                        AND instr(?2, '|' || hex(tb.task_id) || hex(tb.blocker_id) || '|') = 0
                        AND (b.id IS NULL OR (b.deleted = 0
                             AND b.state NOT IN ('done', 'cancelled'))))",
                )?
            }
            IsValue::Stale | IsValue::Fixed | IsValue::Flexible => false,
        })
    }

    fn has(&mut self, v: HasValue, kind: SearchKind, c: &Candidate) -> rusqlite::Result<bool> {
        let task = kind == SearchKind::Task;
        let Some(a) = self.attrs.get(&c.id).cloned() else {
            return Ok(false);
        };
        Ok(match v {
            HasValue::Note => a.has_note,
            HasValue::Estimate => task && a.estimate,
            HasValue::Deadline => task && a.hard.is_some(),
            HasValue::Attachment => self.flag(
                &c.id,
                1,
                "SELECT EXISTS (SELECT 1 FROM attachments WHERE parent_id = ?1 AND deleted = 0)",
            )?,
            HasValue::Dependency => {
                task && self.edge_flag(
                    &c.id,
                    2,
                    "SELECT EXISTS (SELECT 1 FROM task_blockers tb WHERE tb.task_id = ?1
                       AND instr(?2, '|' || hex(tb.task_id) || hex(tb.blocker_id) || '|') = 0)",
                )?
            }
            HasValue::Place => false,
        })
    }
}

fn state_matches(v: StateValue, stored: &str) -> bool {
    match v {
        StateValue::Open => matches!(stored, "todo" | "in_progress"),
        other => other.as_str() == stored,
    }
}

fn priority_matches(cmp: Cmp, values: &[u8], p: u8) -> bool {
    match (cmp, values) {
        (Cmp::Eq, vs) => vs.contains(&p),
        (Cmp::Lt, [v]) => p < *v,
        (Cmp::Le, [v]) => p <= *v,
        (Cmp::Gt, [v]) => p > *v,
        (Cmp::Ge, [v]) => p >= *v,
        _ => false,
    }
}

/// Whether evaluating `node` reads an entity's rows, rather than only the
/// index and the hit's kind.
fn reads_rows(node: &Node) -> bool {
    match node {
        Node::Text(_) | Node::Kinds(_) | Node::Never => false,
        Node::Not(inner) => reads_rows(inner),
        Node::Or(items) | Node::And(items) => items.iter().any(reads_rows),
        _ => true,
    }
}

/// `:` within, `<` before the start, `<=` before the end, `>` after the
/// end, `>=` from the start (`search.md` §Dates).
fn date_matches(cmp: Cmp, days: Days, d: Date) -> bool {
    match cmp {
        Cmp::Eq => days.start <= d && d < days.end,
        Cmp::Lt => d < days.start,
        Cmp::Le => d < days.end,
        Cmp::Gt => d >= days.end,
        Cmp::Ge => d >= days.start,
    }
}

/// Builds a [`Node`] tree from a parsed query, resolving names and dates.
struct Compiler<'q> {
    streams: &'q [Named],
    contexts: &'q [Named],
    today: Date,
    week_start: sunrise_domain::Weekday,
    atoms: Vec<Atom>,
    pending: Vec<Pending>,
    /// The word typed last, when it also matches as a prefix.
    last: Option<&'q str>,
    /// Whether the node being compiled is under an odd number of `-`.
    negated: bool,
}

impl Compiler<'_> {
    fn node(&mut self, e: &Expr) -> Node {
        match e {
            Expr::Term(t) => {
                let prefix = self.last.is_some_and(|l| std::ptr::eq(l, t.as_str()));
                self.atom(t, prefix)
            }
            Expr::Phrase(p) => self.atom(p, false),
            Expr::Not(inner) => {
                self.negated = !self.negated;
                let node = self.node(inner);
                self.negated = !self.negated;
                Node::Not(Box::new(node))
            }
            Expr::Or(items) => Node::Or(items.iter().map(|i| self.node(i)).collect()),
            Expr::Group(items) => Node::And(items.iter().map(|i| self.node(i)).collect()),
            Expr::Filter(f) => self.filter(f),
        }
    }

    fn atom(&mut self, text: &str, prefix: bool) -> Node {
        self.atoms.push(Atom {
            text: text.to_owned(),
            prefix,
            ranked: !self.negated,
            words: HashMap::new(),
            tri: HashMap::new(),
            all: HashSet::new(),
        });
        Node::Text(self.atoms.len() - 1)
    }

    fn unsupported(&mut self, what: String) -> Node {
        self.pending.push(Pending {
            kind: NoticeKind::Unsupported,
            detail: what,
        });
        Node::Never
    }

    fn resolve(&mut self, names: &[String], kind: NameKind) -> HashSet<[u8; 16]> {
        let pool = match kind {
            NameKind::Stream => self.streams,
            NameKind::Context => self.contexts,
        };
        let refs: Vec<NamedRef<'_>> = pool
            .iter()
            .map(|(id, name, _)| NamedRef { id: *id, name })
            .collect();
        let mut out = HashSet::new();
        for name in names {
            let mut unresolved = Vec::new();
            match resolve_named(name, &refs, &mut unresolved, kind) {
                Some(id) => {
                    out.insert(*id.bytes());
                }
                None => {
                    let ambiguous = unresolved.iter().any(|u| {
                        matches!(
                            u,
                            sunrise_domain::capture::Unresolved::AmbiguousStream { .. }
                                | sunrise_domain::capture::Unresolved::AmbiguousContext { .. }
                        )
                    });
                    self.pending.push(Pending {
                        kind: if ambiguous {
                            NoticeKind::AmbiguousName
                        } else {
                            NoticeKind::UnknownName
                        },
                        detail: name.clone(),
                    });
                }
            }
        }
        out
    }

    fn filter(&mut self, f: &Filter) -> Node {
        match f {
            Filter::Stream(names) => {
                let roots = self.resolve(names, NameKind::Stream);
                Node::Streams(descendants(self.streams, roots))
            }
            Filter::Context(names) => Node::Contexts(self.resolve(names, NameKind::Context)),

            Filter::State(v) => Node::State(v.clone()),
            Filter::Kind(v) => Node::Kinds(v.clone()),
            Filter::Priority(cmp, v) => Node::Priority(*cmp, v.clone()),
            Filter::Is(values) => {
                let (ok, not): (Vec<IsValue>, Vec<IsValue>) = values.iter().partition(|v| {
                    !matches!(v, IsValue::Stale | IsValue::Fixed | IsValue::Flexible)
                });
                for v in not {
                    self.unsupported(format!("is:{}", v.as_str()));
                }
                Node::Is(ok)
            }
            Filter::Has(values) => {
                if values.contains(&HasValue::Place) {
                    self.unsupported("has:place".to_owned());
                }
                Node::Has(values.clone())
            }
            // No Place entity and no soft deadline yet: both match nothing.
            Filter::Place(_) | Filter::Date(DateField::Target, ..) => {
                self.unsupported(f.to_string())
            }
            Filter::Date(field, cmp, span) => {
                Node::Date(*field, *cmp, span.days(self.today, &self.week_start))
            }
        }
    }
}

/// `roots` and every stream nested under one of them.
fn descendants(streams: &[Named], roots: HashSet<[u8; 16]>) -> HashSet<[u8; 16]> {
    let mut out = roots;
    loop {
        let before = out.len();
        for (id, _, parent) in streams {
            if parent.is_some_and(|p| out.contains(&p)) {
                out.insert(*id.bytes());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// `text` with what FTS5 or C strings would read as syntax removed: quotes
/// and control characters.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '"' && !c.is_control())
        .collect()
}

/// Fill `atom`'s doc sets from both indexes.
fn match_atom(conn: &Connection, atom: &mut Atom) -> rusqlite::Result<()> {
    let text = clean(&atom.text);
    // A phrase of no word matches nothing in the word index, and a prefix
    // query of one is an error; only text with a letter or digit is sent.
    if text.chars().any(char::is_alphanumeric) {
        let expr = if atom.prefix {
            format!("\"{text}\"*")
        } else {
            format!("\"{text}\"")
        };
        if atom.ranked {
            let mut stmt = conn.prepare(&format!(
                "SELECT rowid, {BM25} FROM search_words WHERE search_words MATCH ?1"
            ))?;
            let rows =
                stmt.query_map([&expr], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
            for r in rows {
                let (doc, score) = r?;
                atom.words.insert(doc, score);
                atom.all.insert(doc);
            }
        } else {
            let mut stmt =
                conn.prepare("SELECT rowid FROM search_words WHERE search_words MATCH ?1")?;
            let rows = stmt.query_map([&expr], |r| r.get::<_, i64>(0))?;
            for r in rows {
                atom.all.insert(r?);
            }
        }
    }
    let folded = clean(&fold(&text));
    let len = folded.chars().count();
    if len >= 3 {
        let quoted = format!("\"{folded}\"");
        let mut only_here = false;
        let mut stmt = conn.prepare("SELECT rowid FROM search_tri WHERE search_tri MATCH ?1")?;
        let rows = stmt.query_map([&quoted], |r| r.get::<_, i64>(0))?;
        for r in rows {
            let doc = r?;
            only_here |= !atom.words.contains_key(&doc);
            atom.all.insert(doc);
        }
        // The trigram `bm25` orders only the hits the word index did not
        // find, so it is computed only when there are some: a word query,
        // the common case, does not pay for it.
        if atom.ranked && only_here {
            let mut stmt = conn.prepare(
                "SELECT rowid, bm25(search_tri, 10.0, 1.0) FROM search_tri
                 WHERE search_tri MATCH ?1",
            )?;
            let rows = stmt.query_map([&quoted], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })?;
            for r in rows {
                let (doc, score) = r?;
                if !atom.words.contains_key(&doc) {
                    atom.tri.insert(doc, score);
                }
            }
        }
    } else if len > 0 && folded.chars().any(is_cjk) {
        let pattern = format!(
            "%{}%",
            folded
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt =
            conn.prepare("SELECT rowid FROM search_tri WHERE title LIKE ?1 ESCAPE '\\' LIMIT ?2")?;
        let rows = stmt.query_map(params![pattern, SHORT_CJK_CAP], |r| r.get::<_, i64>(0))?;
        for r in rows {
            atom.all.insert(r?);
        }
    }
    Ok(())
}

/// The hit a candidate shows as: its indexed title and body, highlighted,
/// and what it opens.
fn hit_of(
    conn: &Connection,
    kind: SearchKind,
    c: &Candidate,
    needles: &[&str],
) -> Result<Option<SearchHit>, EngineError> {
    let Some(ek) = entity_kind(kind) else {
        return Ok(None);
    };
    let Some((mut title, body)) = conn
        .query_row(
            "SELECT title, body FROM search_words WHERE rowid = ?1",
            [c.doc],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let entity = EntityRef::new(ek, c.id);
    let mut opens = entity;
    match kind {
        SearchKind::Block if title.is_empty() => {
            if let Some(b) = read_block(conn, &c.id)? {
                title = block_row(conn, b)?.title.unwrap_or_default();
            }
        }
        SearchKind::Attachment => {
            if let Some(a) = read_attachment(conn, &c.id)? {
                opens = a.parent;
            }
        }
        _ => {}
    }
    let ranges = match_ranges(&title, needles);
    Ok(Some(SearchHit {
        entity,
        opens,
        title: Highlighted {
            text: title,
            ranges,
        },
        snippet: snippet(&body, needles),
    }))
}
