-- 0014_stream_sort_order.sql — persist `Stream.sort_order`.
--
-- The field has been in `docs/02-domain/streams.md` and on the Rust `Stream`
-- since the beginning, but it had no column: `read_stream` synthesized the
-- literal 'a0' for every row, so every stream sorted identically and the
-- sidebar fell back to name order. This is the column that makes the
-- fractional index real. See `sunrise_domain::sort_order`.
--
-- The first migration appended after the ADR-0018 baseline reset. It follows
-- the append-only rule that reset reinstated: 0013 is untouched.

-- '' means "never ordered". It is deliberately NOT a valid key
-- (`sort_order::is_valid` rejects it): it sorts before every real key, which
-- is where an un-keyed row already was under the old name ordering, and it
-- keeps "no position" distinguishable from "first position". Rows inserted by
-- `ensure_stream_row` — placeholders for a stream referenced by an op that has
-- not arrived yet — keep taking it.
ALTER TABLE streams ADD COLUMN sort_order TEXT NOT NULL DEFAULT '';

-- Backfill, so no existing vault has to live through a sidebar where the first
-- drag has no neighbours to compute against. `between(a, b)` needs both of a
-- dragged row's neighbours to hold keys; leaving every pre-existing row at ''
-- would make the very first reorder impossible rather than merely awkward.
--
-- Keys are assigned in the order these rows were already being displayed —
-- `ORDER BY name COLLATE NOCASE, stream_id`, exactly what `query_stream_list`
-- used before this migration — so a user's sidebar does not rearrange itself
-- on upgrade. That is the whole point of ordering the window this way.
--
-- The key for row n (0-based) is n in base 26 over 'A'..'Z', three digits
-- wide, with a trailing 'N':
--
--   * three fixed-width digits, so the strings sort in the same order as the
--     numbers — 'AAB' < 'AAC' < 'ABA' — for up to 17576 streams;
--   * the trailing 'N' keeps every backfilled key off the trailing-'A' rule in
--     `sunrise_domain::sort_order`, so no two of them can be two spellings of
--     one number ('AB' and 'ABA' are the same number, and a pair like that
--     would be un-orderable by string comparison).
--
-- 65 is 'A' and 78 is 'N'.
UPDATE streams
SET sort_order = (
    SELECT char(
        65 + (r.n / 676) % 26,
        65 + (r.n /  26) % 26,
        65 + (r.n        % 26),
        78
    )
    FROM (
        SELECT stream_id,
               row_number() OVER (
                   ORDER BY name COLLATE NOCASE ASC, stream_id ASC
               ) - 1 AS n
        FROM streams
    ) AS r
    WHERE r.stream_id = streams.stream_id
);
