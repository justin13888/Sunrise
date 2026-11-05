-- 0040: two full-text indexes over every searchable entity kind, in place of
-- the task-only `search_idx` (issue #345, ADR-0052,
-- docs/08-features/search.md).
--
-- Why
-- ---
--
-- `search_idx` held tasks only, tokenized `porter unicode61` whatever language
-- the vault was written in, so CJK text could not be split into words and
-- nothing matched inside a word. Its `id` column was UNINDEXED, so replacing
-- one task's row scanned the whole index.
--
-- What
-- ----
--
-- `search_docs` names each indexed entity once, by `(kind, id)`, and its
-- `doc` is the rowid of that entity's row in both indexes, so a row is
-- replaced by key rather than found by a scan.
--
-- `search_words` is the ranked word index: `unicode61 remove_diacritics 2`,
-- with `porter` in front only when the vault's content language is English.
-- This file creates it stemmed, as the old index was; the first open after it
-- rebuilds both indexes from the entity rows with the tokenizer the content
-- language asks for, because `search_meta` holds no row. Columns: `title`
-- (titles and names), `aux` (a block's location, a task's context names, an
-- attachment's filename) and `body` (note text), weighted 10, 5 and 1.
--
-- `search_tri` is the `trigram` index, for CJK text and substrings. It holds
-- text the core folded first (NFKD, combining marks dropped, lowercased),
-- because the SQLite SQLCipher bundles has no `remove_diacritics` for
-- `trigram`: titles and names in full, and bodies up to their first 4 KiB.
--
-- `search_meta` records how the indexes were built: the index layout version,
-- whether `search_words` stems, and the content language it was built for,
-- and, while a background re-index for a new language is running, what it is
-- building and how far it has got.
--
-- Both indexes and `search_docs` are projections: a projection rebuild clears
-- and refolds them. `search_meta` is not, and a rebuild keeps it. No row of
-- `search_idx` is carried: the rebuild at the next open writes every kind's
-- rows from the entities, which `search_idx` never held.

DROP TABLE search_idx;

CREATE TABLE search_docs (
    doc   INTEGER PRIMARY KEY,
    kind  TEXT NOT NULL,
    id    BLOB NOT NULL,
    UNIQUE (kind, id)
);

CREATE VIRTUAL TABLE search_words USING fts5 (
    title,
    aux,
    body,
    tokenize = 'porter unicode61 remove_diacritics 2'
);

CREATE VIRTUAL TABLE search_tri USING fts5 (
    title,
    body,
    tokenize = 'trigram'
);

CREATE TABLE search_meta (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    index_v         INTEGER NOT NULL,
    stemmed         INTEGER NOT NULL,
    language        TEXT,
    reindex_stemmed INTEGER,
    reindex_cursor  INTEGER NOT NULL DEFAULT 0
);
