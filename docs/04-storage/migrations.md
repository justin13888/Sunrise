---
status: accepted
---

# Migrations

> **Pre-1.0 baseline, and appends on top of it.** `BASELINE_STORAGE_V` is
> **13**: migrations 0001–0012 were collapsed into
> `crates/sunrise-storage/migrations/0013_baseline.sql` and deleted, and a vault
> stamped `0 < storage_v < 13` is **refused**
> (`DbError::StorageVPreBaseline` → `STORAGE_V_TOO_OLD`) rather than upgraded.
> That number is frozen — ADR-0018 collapses once — so it is the one this page
> states.
>
> **The current `STORAGE_V` is deliberately not written here.** It is
> `crates/sunrise-cbor/src/version.rs`'s constant, it equals the highest-numbered
> file in
> [`crates/sunrise-storage/migrations/`](../../crates/sunrise-storage/migrations/),
> and `migrations.rs`'s `current_storage_v_matches_the_version_constant` fails
> the build if those two ever disagree. So is the number of migrations: it is
> the file count of that directory. A count in prose has to be hand-edited every
> time a migration lands, which means it is wrong from the next merge until
> someone notices — this page carried "`STORAGE_V` is **16** … four files"
> through four appends, which is the whole argument for not saying it again.
> Read the directory; it cannot drift from itself.
>
> Every file after 0013 was appended, and 0013 was not touched to make room for
> any of them — which is exactly the append-only rule the reset reinstated. The
> runner, the ordering rule and the single-transaction guarantee are unchanged.
> See [ADR-0018](../11-adr/0018-storage-baseline-reset.md) for why the collapse
> was done once, and why it does not happen again after 1.0.

Two kinds of migrations:

1. **Local DB migrations.** Schema changes in SQLite. Run on app launch.
2. **Doc-schema migrations.** Changes to the entity/field shapes (`DOC_SCHEMA_V`). Coordinated across devices.

## Local DB migrations

Each migration:

- Has a numeric version (`db_schema_version`), migrations applied in order.
- Runs **at most once per vault**, and the *runner* is what guarantees that —
  not the SQL. See §Who provides the no-op below before writing one.
- Is forward-only. No down-migration. (Restore from backup if a migration is wrong.)
- Lives in `crates/sunrise-storage/migrations/<NNNN>_<name>.sql` *or* a Rust function for non-trivial transforms.

### Who provides the no-op

This page used to say each migration "is idempotent (re-running is a no-op)".
**That is false of the SQL and always was.** Not one migration in the tree is
re-runnable on its own: they are bare `ALTER TABLE … ADD COLUMN` and
`CREATE TABLE` statements, and SQLite fails the second one with "duplicate
column name" or "table already exists". Replay 0017 and it renames a
`stream_keys` that is already the new shape.

The property is real, but it comes from the runner, twice over:

1. `Db::run_migrations` applies `MIGRATIONS.iter().filter(|m| m.id > from_v)`,
   where `from_v` is the vault's stamped `storage_v`. A migration at or below
   the stamp is never handed to SQLite at all.
2. `Db::ensure_schema` only calls the runner `if db_v < binary_v`, so a current
   vault does no work.

The guard is genuinely doubled: mutating the outer comparison to `<=` still
produces a no-op, because the inner filter catches it.

Why this distinction is worth a section rather than a footnote: it decides what
a migration author is allowed to write. Someone who believes their SQL will be
replayed writes defensively — `IF NOT EXISTS`, `INSERT OR IGNORE`, a
re-runnable `UPDATE` — and someone who knows it will not writes the direct
statement and lets it fail loudly if it ever runs twice, which is what every
migration here does. The contract you are writing to is **exactly once, inside
one transaction, forward only**.

Framework: a custom thin layer (we don't use `refinery` because we want the
transaction and the version write under our own control). One transaction spans
the **whole batch**, not one per migration — a 13-era vault reaching the current
version either arrives there or is left untouched at 13, and never rests at an
intermediate version no build was ever tested against.

Sample — a whole migration file, and nothing else:

```sql
-- 00NN_add_task_energy.sql
ALTER TABLE tasks ADD COLUMN energy TEXT;
```

The file does **not** stamp the version. `Db::run_migrations` writes
`schema_meta.storage_v` once, after the last migration in the batch, inside the
same transaction — so a migration that stamped it itself would be overwritten
by the runner anyway, and a chain of five would stamp four versions no vault
ever rested at. (The column is `storage_v`; there has never been a
`db_schema_version` column in this schema.)

Migrations run in a single transaction; failure rolls back; the app refuses to launch on migration failure and surfaces a clear error with a "send diagnostics" path.

### Failure recovery

- Failure mid-migration leaves the previous schema intact (migrations run in a single transaction; ROLLBACK on error).
- The app refuses to launch with `STORAGE_MIGRATION_FAILED { from_v, to_v, error_code }`. Diagnostic-bundle export is allowed; vault data access is not.
- Recovery options surfaced in UI:
  1. **Retry** — re-run the migration; useful for transient I/O errors.
  2. **Restore from backup** — if user has an external backup of the vault directory.
  3. **Reset and resync** — delete local vault, re-pair the device; remote ops are intact.
- Downgrades require an explicit reverse migration with its own ADR; v1 ships no reverse migrations.

## Doc-schema migrations

These are coordinated:

1. **Add a field.** Trivial; old clients don't see it. Bump `doc_schema_version`.
2. **Remove a field.** Two-stage:
   - Stage A: stop reading. Bump version A.
   - Stage B: stop writing. Bump version B (after the A-version is the minimum across all known devices for the user).
3. **Rename a field.** Add new + dual-write + stop reading old + stop writing old. Or keep both forever.
4. **Change a type.** Treated as remove + add. Rare; only with extreme care.

The user's vault stores the highest `doc_schema_version` it has seen any op produced under. Devices on older versions will refuse to *originate* ops under a newer version — they read but don't write past their max.

This produces a property: a v1 device and a v2 device coexist, with the v1 device gracefully degrading.

> **Neither half of that paragraph is implemented.** The vault records
> `storage_v` in `schema_meta` and nothing else: **there is no stored highest
> `doc_schema_version` seen**, and consequently **no gate stopping an older
> build from originating ops under a newer version**. Every op this build emits
> carries `DOC_SCHEMA_V` from `crates/sunrise-cbor/src/version.rs`, and the only
> check on the receive side is the floor. The degradation property holds for a
> different reason — forward-compatible field preservation — not because
> anything refuses to write.
>
> §Minimum-version discovery below is target state for the same reason: there
> are no cursor heartbeats carrying `doc_schema_v_max`, no 7-day grace, and no
> server-side `doc_schema_floor` that advances.

### Minimum-version discovery

The "minimum `DOC_SCHEMA_V` across all known devices" is determined from device-cursor heartbeats. Each cursor op carries `doc_schema_v_max` (the highest version that device can write).

- After all active devices' cursors report `doc_schema_v_max ≥ N+1`, a 7-day grace begins.
- After grace, the **server's `doc_schema_floor`** advances to `N+1`, refusing inbound ops with `doc_schema_v < N+1`.
- A device that has not heartbeat in 30 days is considered abandoned and excluded from the minimum.

## Materialized state rebuild

> **Not implemented, at any layer.** There is no rebuild path in the workspace:
> no command, no seam function, no CLI verb, and no internal helper — `rebuild`
> appears nowhere in `crates/`. The op log genuinely does hold everything needed
> (every op is full-state under
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md), and `ops.envelope` keeps
> the sealed bytes), so the capability is buildable; nothing has built it. Today
> the recovery answer for a corrupt projection is §Failure recovery's option 3,
> "reset and resync".

If a migration is irrecoverable (a corrupt index, a bug), the user can trigger "rebuild from log":

1. Wipe materialized tables.
2. Re-apply ops in causal order.
3. Rebuild FTS index.

This is an internal capability used by the app on first launch after major-version upgrades when materialization logic changes.

## No op-log migration

**The op log is not migrated, and cannot be.** Ops are decoded by whatever
`InnerOp` definition this build was compiled with; there is no stored op
version, no rewrite pass, and no translation layer. That splits into two rules
with opposite consequences:

- **A new field is forward-compatible.** An older build decodes the op, keeps
  the field it does not model in the entity's `unknown` map, and re-emits it —
  see [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md).
  This is the ordinary `DOC_SCHEMA_V` bump.
- **A new op FAMILY is not.** A build handed an `InnerOp` variant it does not
  know cannot decode it at all — a new variant is not a new field — and reports
  it as an invalid remote op rather than applying it wrongly. There is no
  migration that can help, because the bytes describe an operation this binary
  has no code for.

[ADR-0024](../11-adr/0024-key-hierarchy.md) has landed three of them —
`key_envelope`, `device_revoke` and `device_cert`, at `DOC_SCHEMA_V = 5` — and
[ADR-0025](../11-adr/0025-integration-account-entity.md) (the
`IntegrationAccount` family) will add another, so both are breaking changes to
the op vocabulary. That is acceptable pre-1.0 under
[ADR-0018](../11-adr/0018-storage-baseline-reset.md), where no older build
exists — and it is recorded here rather than discovered later, because after
1.0 the same change needs a flag day.

Migration `0017` is the storage half of the same ADR, and it is the one
migration so far that **drops** rows rather than adding to them: `stream_keys`
is re-keyed on `(stream_id, epoch, key_id)`, and the pre-0017 rows held keys
*derived* from the vault root, which the new schedule does not use. They are
not migrated because they do not need to be — `Keychain::open` recomputes the
old derivation once, for exactly the streams that have ops sealed under it, and
files the result at epoch 1 with `source = 'legacy'`. Everything else 0017 adds
(the `identity` and `deferred_ops` tables, four columns on `devices`, one on
`streams`) is additive and needs no backfill, on the 0016 precedent.

## Migration testing

`crates/sunrise-storage/src/db.rs` and `migrations.rs` assert:

- a fresh vault applies all of them and lands at `STORAGE_V`, with the tables
  the collapse was supposed to preserve and without the schema it was supposed
  to drop;
- every `storage_v` in `1..13` is refused with `StorageVPreBaseline`, and a
  `storage_v` above `STORAGE_V` with `StorageVTooNew`;
- 0014 adds `streams.sort_order` defaulting to the "never ordered" empty
  string, and its backfill hands every pre-existing row a key **in the order
  those rows were already being displayed** — asserted by replaying 0013 and
  then 0014 by hand, because a fresh vault has no rows for a backfill to touch,
  which is precisely the case a fresh-vault test cannot cover;
- 0017 creates `identity` and `deferred_ops`, re-keys `stream_keys` on
  `(stream_id, epoch, key_id)`, and leaves that table **empty** — asserted by
  replaying up to 0016, writing a pre-hierarchy key row by hand, then applying
  0017, because the drop is the part a fresh-vault test cannot see;
- migration ids strictly ascend, and `current_storage_v()` equals the
  `STORAGE_V` constant, so the list and the constant cannot drift apart.

Every one of those replays SQL into a connection this build just created. None
of them opens an *old vault*, and for a long time nothing did — which meant the
migrations had never run against the thing they exist for.

### Old-vault fixtures

`crates/sunrise-storage/fixtures/` holds encrypted SQLCipher vaults written at
historical `STORAGE_V`s.
[`src/vault_fixtures.rs`](../../crates/sunrise-storage/src/vault_fixtures.rs)
generates them, states their contents, copies each to a temporary directory,
opens the copy with the ordinary public `Db::open`, and asserts on the contents
afterwards — **not** on the open returning `Ok`. A migration that runs clean and
drops a column's data passes a "does it open" test, so success is not the
assertion: the rows are.

Three decisions are worth knowing before adding one.

- **The vault root key is committed beside the fixtures**, and that is correct.
  An encrypted fixture is useless without its key, and an unencrypted one does
  not go through the `PRAGMA key` path a real vault does. It guards invented
  rows and has never keyed anything else; `fixtures/README.md` says so where a
  reader who finds the binary first will see it.
- **The fixtures are regenerable**, by `mise run storage-fixtures`, which runs
  an `#[ignore]`d test so an ordinary `cargo test` reads the committed files
  rather than rewriting them. Regenerate rarely: a fixture rewritten at today's
  schema is no longer old, and the chain it exists to exercise then runs over
  nothing. One test asserts the v13 file is still stamped 13 for exactly that
  reason.
- **Not every version needs one.** The rule is the oldest supported version,
  plus any later version whose successor migrations move *data* the older
  fixture cannot contain. That is 13 and 17 today. Of the appended migrations,
  three move data rather than only schema — 0014's `sort_order` backfill,
  0017's `stream_keys` drop / `device_revocations` carry / Inbox re-point, and
  0019's `minted_by_device_id` backfill and `id_d_priv_wrapped` blanking — and
  the v13 fixture reaches the first two but structurally cannot reach the third,
  because `identity` does not exist before 0017 and 0017 creates it empty. 0015,
  0016, 0018, 0020 and 0021 are pure `ADD COLUMN` / `CREATE TABLE` /
  `CREATE INDEX` and earn no fixture of their own.

What is still **specified and not implemented**:

- CI runs every migration over every prior fixture. There is no such job; the
  fixtures are ordinary `cargo test` unit tests, which is why they run at all.
- A "fuzz" job randomly orders migrations against random data states. Nothing
  like it exists, and it is a doubtful fit — the runner applies ids in ascending
  order and a random order is not a state any vault can reach.
