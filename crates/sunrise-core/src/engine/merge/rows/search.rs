//! The full-text rows: what every indexed entity's projection writes to the
//! two search indexes (ADR-0052 §1–§2, `docs/08-features/search.md`).
//!
//! An entity is indexed under one `search_docs` row, keyed `(kind, id)`, and
//! its `doc` is the rowid of its row in `search_words` (the ranked word
//! index) and in `search_tri` (the trigram index, over folded text). Each row
//! writer of an indexed kind calls [`index_doc`] or [`unindex`] in the
//! transaction that writes the entity's row, so the index is always the
//! projection of the rows beside it, and a tombstone removes the entity's
//! row from both.
//!
//! What each kind contributes is [`doc_of_task`] and its siblings, which the
//! rebuild from rows ([`rebuild_from_rows`]) uses too, so the two ways of
//! writing the index cannot write different text.
//!
//! While a background re-index for a new content language runs
//! (`engine::search`), every write to a doc it has already copied is also
//! made to the index it is building, `search_words_next`.

use crate::engine::attachment::read_attachment;
use crate::engine::block::read_block;
use crate::engine::context::read_context;
use crate::engine::ids::blob16;
use crate::engine::stream::read_stream;
use crate::engine::task::read_task;
use crate::engine::EngineError;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_domain::search::{fold, SearchKind};
use sunrise_domain::{
    decode_note_body, note_body_to_plain_text, Attachment, Block, Context, NoteBody, SavedView,
    Stream, Task,
};

/// How much of a body the trigram index holds, in bytes of folded text
/// (ADR-0052 §2). Past it a body is found by whole words only.
pub(in crate::engine) const TRIGRAM_BODY_BYTES: usize = 4096;

/// The tokenizer of `search_words`: `unicode61` folding diacritics, with
/// Porter stemming in front when the content language is English.
pub(in crate::engine) fn words_tokenizer(stemmed: bool) -> &'static str {
    if stemmed {
        "porter unicode61 remove_diacritics 2"
    } else {
        "unicode61 remove_diacritics 2"
    }
}

/// The SQL that creates a word index named `table`.
pub(in crate::engine) fn create_words_sql(table: &str, stemmed: bool) -> String {
    format!(
        "CREATE VIRTUAL TABLE {table} USING fts5 (title, aux, body, tokenize = '{}')",
        words_tokenizer(stemmed)
    )
}

/// One entity's text, as both indexes hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::engine) struct SearchDoc {
    pub(in crate::engine) kind: SearchKind,
    pub(in crate::engine) id: [u8; 16],
    /// The title or name: weighted highest, and trigram-indexed in full.
    pub(in crate::engine) title: String,
    /// A task's context names, weighted between title and body.
    pub(in crate::engine) aux: String,
    /// The note text, as plain text.
    pub(in crate::engine) body: String,
}

fn plain(body: Option<&NoteBody>) -> String {
    body.filter(|b| !b.is_empty())
        .map(|b| note_body_to_plain_text(&decode_note_body(b)))
        .unwrap_or_default()
}

/// A task: its title, the names of the live contexts it carries, and its
/// note. Context names are read from their rows, in name order, so a rename
/// is a rewrite of every task that carries the context ([`reindex_tasks_of`]).
pub(in crate::engine) fn doc_of_task(
    tx: &Transaction<'_>,
    t: &Task,
) -> rusqlite::Result<SearchDoc> {
    let mut names = Vec::with_capacity(t.contexts.len());
    let mut stmt = tx.prepare("SELECT name FROM contexts WHERE id = ? AND deleted = 0")?;
    for c in &t.contexts {
        if let Some(name) = stmt
            .query_row(params![&c.bytes()[..]], |r| r.get::<_, String>(0))
            .optional()?
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(SearchDoc {
        kind: SearchKind::Task,
        id: *t.id.bytes(),
        title: t.title.clone(),
        aux: names.join(" "),
        body: plain(t.body.as_ref()),
    })
}

/// A stream: its name and description.
pub(in crate::engine) fn doc_of_stream(s: &Stream) -> SearchDoc {
    SearchDoc {
        kind: SearchKind::Stream,
        id: *s.id.bytes(),
        title: s.name.clone(),
        aux: String::new(),
        body: plain(s.description.as_ref()),
    }
}

/// A context: its name, and its description as body text.
pub(in crate::engine) fn doc_of_context(c: &Context) -> SearchDoc {
    SearchDoc {
        kind: SearchKind::Context,
        id: *c.id.bytes(),
        title: c.name.clone(),
        aux: String::new(),
        body: c.description.clone().unwrap_or_default(),
    }
}

/// A block: its own title. A block that takes its title from its task is
/// found through the task, and by its filters.
pub(in crate::engine) fn doc_of_block(b: &Block) -> SearchDoc {
    SearchDoc {
        kind: SearchKind::Block,
        id: *b.id.bytes(),
        title: b.title.clone().unwrap_or_default(),
        aux: String::new(),
        body: String::new(),
    }
}

/// An attachment: its filename, never its contents.
pub(in crate::engine) fn doc_of_attachment(a: &Attachment) -> SearchDoc {
    SearchDoc {
        kind: SearchKind::Attachment,
        id: *a.id.bytes(),
        title: a.filename.clone(),
        aux: String::new(),
        body: String::new(),
    }
}

/// A saved view: its name, and its filter's free text as body text.
pub(in crate::engine) fn doc_of_saved_view(v: &SavedView) -> SearchDoc {
    SearchDoc {
        kind: SearchKind::View,
        id: *v.id.bytes(),
        title: v.name.clone(),
        aux: String::new(),
        body: v.filter.text.clone(),
    }
}

/// Index a saved view.
pub(in crate::engine) fn index_saved_view(
    tx: &Transaction<'_>,
    v: &SavedView,
) -> rusqlite::Result<()> {
    index_doc(tx, &doc_of_saved_view(v))
}

/// `text` folded, cut to at most `max` bytes on a character boundary.
fn folded_prefix(text: &str, max: usize) -> String {
    let mut f = fold(text);
    if f.len() > max {
        let mut cut = max;
        while !f.is_char_boundary(cut) {
            cut -= 1;
        }
        f.truncate(cut);
    }
    f
}

/// Whether a background re-index is building `search_words_next`, and the
/// last doc it has copied into it.
fn reindex_cursor(tx: &Transaction<'_>) -> rusqlite::Result<Option<i64>> {
    tx.prepare(
        "SELECT reindex_cursor FROM search_meta
         WHERE id = 1 AND reindex_stemmed IS NOT NULL",
    )?
    .query_row([], |r| r.get(0))
    .optional()
}

fn doc_id(tx: &Transaction<'_>, kind: SearchKind, id: &[u8; 16]) -> rusqlite::Result<Option<i64>> {
    tx.prepare("SELECT doc FROM search_docs WHERE kind = ? AND id = ?")?
        .query_row(params![kind.as_str(), &id[..]], |r| r.get(0))
        .optional()
}

fn delete_text(tx: &Transaction<'_>, doc: i64, mirrored: bool) -> rusqlite::Result<()> {
    tx.prepare("DELETE FROM search_words WHERE rowid = ?")?
        .execute([doc])?;
    tx.prepare("DELETE FROM search_tri WHERE rowid = ?")?
        .execute([doc])?;
    if mirrored {
        tx.prepare("DELETE FROM search_words_next WHERE rowid = ?")?
            .execute([doc])?;
    }
    Ok(())
}

/// Write `doc` to both indexes, replacing what its entity had there.
pub(in crate::engine) fn index_doc(tx: &Transaction<'_>, doc: &SearchDoc) -> rusqlite::Result<()> {
    let cursor = reindex_cursor(tx)?;
    let rowid = if let Some(rowid) = doc_id(tx, doc.kind, &doc.id)? {
        delete_text(tx, rowid, cursor.is_some_and(|c| rowid <= c))?;
        rowid
    } else {
        tx.prepare("INSERT INTO search_docs (kind, id) VALUES (?, ?)")?
            .execute(params![doc.kind.as_str(), &doc.id[..]])?;
        tx.last_insert_rowid()
    };
    tx.prepare("INSERT INTO search_words (rowid, title, aux, body) VALUES (?, ?, ?, ?)")?
        .execute(params![rowid, doc.title, doc.aux, doc.body])?;
    tx.prepare("INSERT INTO search_tri (rowid, title, body) VALUES (?, ?, ?)")?
        .execute(params![
            rowid,
            fold(&doc.title),
            folded_prefix(&doc.body, TRIGRAM_BODY_BYTES)
        ])?;
    if cursor.is_some_and(|c| rowid <= c) {
        tx.prepare("INSERT INTO search_words_next (rowid, title, aux, body) VALUES (?, ?, ?, ?)")?
            .execute(params![rowid, doc.title, doc.aux, doc.body])?;
    }
    Ok(())
}

/// Remove an entity from both indexes. Idempotent.
pub(in crate::engine) fn unindex(
    tx: &Transaction<'_>,
    kind: SearchKind,
    id: &[u8; 16],
) -> rusqlite::Result<()> {
    if let Some(rowid) = doc_id(tx, kind, id)? {
        let cursor = reindex_cursor(tx)?;
        delete_text(tx, rowid, cursor.is_some_and(|c| rowid <= c))?;
        tx.prepare("DELETE FROM search_docs WHERE doc = ?")?
            .execute([rowid])?;
    }
    Ok(())
}

/// Index a task, or remove it when it is tombstoned.
pub(in crate::engine) fn index_task(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    if t.deleted {
        unindex(tx, SearchKind::Task, t.id.bytes())
    } else {
        index_doc(tx, &doc_of_task(tx, t)?)
    }
}

/// Index a stream, or remove it when it is tombstoned.
pub(in crate::engine) fn index_stream(tx: &Transaction<'_>, s: &Stream) -> rusqlite::Result<()> {
    if s.deleted {
        unindex(tx, SearchKind::Stream, s.id.bytes())
    } else {
        index_doc(tx, &doc_of_stream(s))
    }
}

/// Index a context, or remove it when it is tombstoned, and rewrite every
/// task carrying it, whose text holds its name.
pub(in crate::engine) fn index_context(tx: &Transaction<'_>, c: &Context) -> rusqlite::Result<()> {
    if c.deleted {
        unindex(tx, SearchKind::Context, c.id.bytes())?;
    } else {
        index_doc(tx, &doc_of_context(c))?;
    }
    reindex_tasks_of(tx, c.id.bytes())
}

/// Index a block, or remove it when it is tombstoned.
pub(in crate::engine) fn index_block(tx: &Transaction<'_>, b: &Block) -> rusqlite::Result<()> {
    if b.deleted {
        unindex(tx, SearchKind::Block, b.id.bytes())
    } else {
        index_doc(tx, &doc_of_block(b))
    }
}

/// Index an attachment, or remove it when it is tombstoned.
pub(in crate::engine) fn index_attachment(
    tx: &Transaction<'_>,
    a: &Attachment,
) -> rusqlite::Result<()> {
    if a.deleted {
        unindex(tx, SearchKind::Attachment, a.id.bytes())
    } else {
        index_doc(tx, &doc_of_attachment(a))
    }
}

fn to_sql(e: EngineError) -> rusqlite::Error {
    match e {
        EngineError::Sqlite(e) => e,
        other => rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
            other.to_string(),
        ))),
    }
}

/// Rewrite the index rows of every task carrying `context`, from their
/// rows: its name is part of their text.
pub(in crate::engine) fn reindex_tasks_of(
    tx: &Transaction<'_>,
    context: &[u8; 16],
) -> rusqlite::Result<()> {
    let tasks: Vec<Vec<u8>> = {
        let mut stmt =
            tx.prepare("SELECT task_id FROM task_contexts WHERE context_id = ? ORDER BY 1")?;
        let rows = stmt.query_map(params![&context[..]], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for raw in tasks {
        reindex_task_row(tx, &blob16(&raw))?;
    }
    Ok(())
}

/// Rewrite one task's index rows from its row.
pub(in crate::engine) fn reindex_task_row(
    tx: &Transaction<'_>,
    id: &[u8; 16],
) -> rusqlite::Result<()> {
    match read_task(tx, id).map_err(to_sql)? {
        Some(t) => index_task(tx, &t),
        None => unindex(tx, SearchKind::Task, id),
    }
}

fn ids(tx: &Transaction<'_>, sql: &str) -> rusqlite::Result<Vec<[u8; 16]>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    rows.map(|r| r.map(|raw| blob16(&raw))).collect()
}

/// Drop both indexes and write them again from the entity rows, with a word
/// index that stems when `stemmed` is set. What the fold would write for
/// the same rows: every live entity of an indexed kind, and no placeholder
/// stream, which no op wrote and so no fold indexed.
///
/// Any background re-index is abandoned: what it was building is replaced.
pub(in crate::engine) fn rebuild_from_rows(
    tx: &Transaction<'_>,
    stemmed: bool,
) -> Result<u64, EngineError> {
    tx.execute_batch(
        "DROP TABLE IF EXISTS search_words_next;
         UPDATE search_meta SET reindex_stemmed = NULL, reindex_cursor = 0;
         DELETE FROM search_docs;
         DELETE FROM search_tri;
         DROP TABLE search_words;",
    )?;
    tx.execute_batch(&create_words_sql("search_words", stemmed))?;
    let mut n = 0u64;
    for id in ids(tx, "SELECT id FROM tasks WHERE deleted = 0 ORDER BY id")? {
        if let Some(t) = read_task(tx, &id)? {
            index_task(tx, &t)?;
            n += 1;
        }
    }
    for id in ids(tx, "SELECT id FROM blocks WHERE deleted = 0 ORDER BY id")? {
        if let Some(b) = read_block(tx, &id)? {
            index_block(tx, &b)?;
            n += 1;
        }
    }
    for id in ids(
        tx,
        "SELECT stream_id FROM streams
         WHERE deleted = 0 AND lww_device IS NOT NULL ORDER BY stream_id",
    )? {
        if let Some(s) = read_stream(tx, &id)? {
            index_stream(tx, &s)?;
            n += 1;
        }
    }
    for id in ids(tx, "SELECT id FROM contexts WHERE deleted = 0 ORDER BY id")? {
        if let Some(c) = read_context(tx, &id)? {
            index_doc(tx, &doc_of_context(&c))?;
            n += 1;
        }
    }
    for id in ids(
        tx,
        "SELECT id FROM attachments WHERE deleted = 0 ORDER BY id",
    )? {
        if let Some(a) = read_attachment(tx, &id)? {
            index_attachment(tx, &a)?;
            n += 1;
        }
    }
    for v in crate::engine::saved_view::read_saved_views(tx)? {
        index_saved_view(tx, &v)?;
        n += 1;
    }
    Ok(n)
}
