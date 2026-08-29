# 0018 — The local schema collapses to one pre-1.0 baseline at `STORAGE_V = 13`

**Status:** accepted

**Amends:** [`docs/04-storage/migrations.md`](../04-storage/migrations.md),
whose append-only rule this suspends exactly once and then reinstates.

## Context

Twelve migrations, `0001_init.sql` through `0012_stream_pause_state.sql`,
described the local schema's whole history. Every one of them was written on
this branch, before any release. **No vault has ever existed at any of those
versions except a developer's own**, and `0005_sync_local.sql`'s header already
said as much in prose: "Sunrise is pre-release: WIPE dev vaults before upgrading
to STORAGE_V = 5."

So the sequence was paying the full cost of an append-only history — twelve
files, a reader has to replay them mentally to know what a table looks like —
for none of the benefit, which is upgrading vaults that exist.

It was also carrying schema that nothing reads, and the incremental format is
what made that easy to miss:

* **`streams.doc_blob` / `doc_blob_v`** — the Loro CRDT document from
  `0001_init.sql`. [ADR-0014](./0014-entity-level-lww-merge.md) replaced CRDT
  merge with entity-level LWW and deleted Loro, but the columns were `NOT NULL`,
  so every stream insert in the engine wrote an empty blob and a literal `1` to
  satisfy a constraint on a column with no reader. Nine call sites.
* **`merge_journal`** — a per-**field** conflict journal, for a merge model that
  is entity-level. ADR-0014 says so outright: "the merge journal has nothing to
  record". Zero writers, zero readers, one index.
* **`routines.rrule`** — superseded by `rrule_text` in `0004`, but left
  `NOT NULL`, so the engine dual-wrote the same string into both columns
  forever. `0004`'s own header documents the workaround.
* **`routines.extra`** — `0004` recorded it as "left NULL". It still was.

Meanwhile three changes in this epic — HLC columns, `SunriseTime` sidecars, and
a `tasks.extra` that is actually written — each needed schema, and appending
three more files to a sequence nobody can upgrade from would have made the
problem worse.

## Decision

**`0013_baseline.sql` is the whole schema, and the sole entry in `MIGRATIONS`,
at `STORAGE_V = 13`.** Migrations 0001–0012 are deleted.

**A vault stamped `0 < storage_v < 13` is REFUSED**, with its own typed error:

```rust
DbError::StorageVPreBaseline { db_v, baseline_v }
```

Deliberately not `StorageVTooOld`, which means "run the migrations". The
migrations that would upgrade such a vault no longer exist, so there is nothing
to run and no honest outcome but a fresh vault. Running the baseline over it
would try to `CREATE` tables that already exist; skipping it would hand the
engine a schema missing half its columns. Both are worse than a clear refusal.
It maps to the existing `STORAGE_V_TOO_OLD` wire code, so no error code is added
or removed.

**The migration runner is untouched in substance.** The list collapsed; the
mechanism did not. Ids still ascend, migrations still apply in one transaction,
`STORAGE_V` still equals the last id, and the too-new check is unchanged. The
next schema change appends `0014_*.sql` exactly as it always would have.

**The baseline carries each collapsed migration's rationale.** The `WHY` comments
in 0008, 0010 and 0011 — no foreign keys on the receive path, separate start and
end tables, append-only review snapshots — are the most valuable thing in those
files, and they moved onto the tables they explain rather than dying with the
file they were written in.

## What we give up

* **Every developer with a vault must wipe it.** That is the whole cost, and it
  was already the standing instruction in `0005`'s header.
* **The migration history is gone from the tree.** Git still has it. What is lost
  is the ability to read the schema's evolution by listing a directory, which is
  a real if small loss for archaeology.
* **The append-only rule is suspended once.** Doing it a second time after 1.0
  would be a data-loss event, so this ADR is also the record of why it must not
  be repeated.
* **The baseline is editable for the rest of the pre-1.0 window.** Three commits
  in this epic add columns to it directly rather than appending files. That is
  intentional and it ends at 1.0.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Append 0013, 0014, 0015 for the new columns** | Keeps the rule intact and makes the underlying problem worse: fifteen files describing a schema nobody can upgrade from, still carrying four pieces of dead schema, still requiring a mental replay to know what a table looks like. |
| **Collapse, and silently ignore old vaults** (skip the baseline when `schema_meta` exists) | Opens a v12 vault against v13 code with columns missing. Every failure surfaces later, as a confusing SQL error in an unrelated query. |
| **Collapse, and attempt an in-place upgrade** (`ALTER` a v12 vault up to the baseline) | Reintroduces the migration history as Rust instead of SQL, to serve zero real vaults. |
| **Collapse, refuse pre-baseline vaults with a typed error (chosen)** | One file, one version, one clear failure with one clear remedy. |

## Consequences

* **`STORAGE_V = 13`, and `BASELINE_STORAGE_V = 13` is the floor.** A vault at
  or above the floor and below `STORAGE_V` still upgrades normally — the runner
  is intact — there simply is not one yet.
* **`db.migrate.refused` is a new logged event.** Refusing to open a vault is a
  thing a user cannot diagnose from the UI, because there is no UI yet.
* **The dead schema is gone and tested gone.** `baseline_omits_the_dead_schema`
  asserts `merge_journal`, `streams.doc_blob`, `streams.doc_blob_v`,
  `routines.rrule` and `routines.extra` are absent, so a future migration cannot
  reintroduce them by copy-paste.
* **The eleven per-version upgrade tests went with the migrations they
  exercised.** In their place the baseline is asserted directly: the tables it
  must create, the schema it must not recreate, and a refusal for every
  pre-baseline version.

## What would force revisiting this

1. **1.0.** At release the baseline freezes and the append-only rule resumes
   unconditionally. This ADR is not a precedent for a second reset.
2. **A pre-1.0 vault worth preserving** — a beta programme, a dogfooding
   dataset. At that point "wipe it" stops being free and the next schema change
   is an appended migration, whatever else is in the file.

---

## Addendum — `0014_stream_sort_order.sql` landed (post-acceptance)

**Nothing above is retracted. Two of its sentences have simply stopped
describing the present, and this note says which, so a reader does not take a
frozen description for a live one.**

`0014_stream_sort_order.sql` is the first migration appended after this reset.
It adds `streams.sort_order` — the column behind `Stream.sort_order`, which
until then had no storage at all and was synthesized as the literal `"a0"` on
every read — and backfills existing rows in the order the sidebar was already
displaying them. `STORAGE_V` is now **14**.

Overtaken as descriptions of the present, not as decisions:

* **"`0013_baseline.sql` is the whole schema, and the sole entry in
  `MIGRATIONS`."** It is the *baseline* and the *first* entry. The collapse
  itself stands: 0001–0012 are still deleted, 0013 was not touched to make room
  for 0014, and the append-only rule this ADR suspended once and reinstated is
  what 0014 obeys. The Decision's own next sentence anticipated this exactly —
  "the next schema change appends `0014_*.sql` exactly as it always would have"
  — so this is the ADR working, not the ADR failing.
* **"A vault at or above the floor and below `STORAGE_V` still upgrades
  normally … there simply is not one yet."** There is one now: a vault at 13
  upgrades to 14. The floor is unchanged.

The consequence worth carrying forward is that **`STORAGE_V` and
`BASELINE_STORAGE_V` have parted company**, and quoting `13` for both — which
several documents did, and which this ADR's own title does — is now ambiguous
in the one place ambiguity costs something: `STORAGE_V` decides whether a vault
is too new, `BASELINE_STORAGE_V` decides whether it is refused outright. The
title is left as written because it names the decision at the version it was
taken.

Neither `DOC_SCHEMA_V` nor any wire constant moved for this. `sort_order` was
already a required `tstr` in the `Stream` payload, carrying `"a0"`; 0014 gave
the field real storage rather than adding a field.
