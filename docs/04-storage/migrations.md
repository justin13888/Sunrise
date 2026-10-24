---
status: accepted
---

# Migrations

> **One-time baseline, and appends on top of it.** `BASELINE_STORAGE_V` is
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
> was done once, and why it is not repeated.

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
- Before the batch starts, the vault has been copied to `<vault>.pre-v<from>.bak`
  (§Migration rigor rule 2), and that copy stays until the next successful open.
- The app refuses to launch with `STORAGE_MIGRATION_FAILED { from_v, to_v, error_code }`. Diagnostic-bundle export is allowed; vault data access is not.
- Recovery options surfaced in UI:
  1. **Retry** — re-run the migration; useful for transient I/O errors.
  2. **Restore from backup** — if user has an external backup of the vault directory.
  3. **Reset and resync** — delete local vault, re-pair the device; remote ops are intact.
- Downgrades require an explicit reverse migration with its own ADR; no reverse migrations exist today.

## Migration rigor

> **Normative, and partly built.** Every rule in this section is a MUST.
> Rules 2 and 3 ship, and so does rule 1 except its rebuild-equivalence check
> ([#327](https://github.com/justin13888/Sunrise/issues/327)). Rule 4's
> rebuild and its table classification ship; running it under a rule-2
> backup, offering it on a failed open, and rule 1's rebuild-equivalence
> check do not ([#441](https://github.com/justin13888/Sunrise/issues/441)). The
> *Today* notes say where the tree falls short. The rules exist because the one invariant every versioned surface
> serves ([ADR-0042](../11-adr/0042-v0-forever.md)) applies to a device's own
> vault file as much as to the merge:
>
> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

A storage migration runs once, unattended, on a user's only local copy. So the
rules below make every migration testable before it ships, recoverable if it
goes wrong, and unable to destroy anything the op log cannot regenerate.

### 1. Every fixture runs through every migration in CI

- **Fixtures.** There MUST be a committed old-vault fixture
  (§Old-vault fixtures) for **every `STORAGE_V` that has shipped in a tagged
  release**, from `BASELINE_STORAGE_V` (13) onward. This replaces the narrower
  "oldest plus data-moving versions" rule below for every version from now on.
  A migration that looks schema-only today has been wrong before. The cost of a
  fixture is one generator function.
- **The CI job.** A dedicated job opens a copy of every fixture with the
  ordinary `Db::open`, which migrates it to the current `STORAGE_V`. It then
  asserts on **row contents**, not on the open succeeding. Each fixture states
  the rows it holds, and the assertions check that every one of them survives
  in its migrated shape.
- **The gate.** Adding a migration `NNNN` without a fixture at `NNNN − 1` fails
  CI.
- **Rebuild equivalence.** After migrating, the job also runs the projection
  rebuild (rule 4). It asserts that the rebuilt projection equals the migrated
  one. A migration that transforms data differently from how the op log would
  re-derive it is a bug, whichever side is wrong.

*Today:* a fixture exists for every `STORAGE_V` from 13 to the one before the
current version, which is wider than the rule: it does not wait for a version
to ship in a tagged release. `every_fixture_keeps_every_row_through_every_later_migration`
in `crates/sunrise-storage/src/vault_fixtures.rs` is the row-content check,
and `every_migration_has_a_fixture_at_the_version_before_it` is the gate. They
run in the `Storage migration fixtures` CI job and in the workspace suite. The
rebuild-equivalence check waits for the rebuild
([#441](https://github.com/justin13888/Sunrise/issues/441)).

### 2. A backup is taken before any migration

- Before `Db::run_migrations` applies a batch, the vault MUST be copied with
  `VACUUM INTO` or the SQLite online-backup API. The copy goes into the vault's
  own directory as `<vault>.pre-v<from>.bak`, encrypted under the same SQLCipher
  key, and it is fsynced.
- **No backup, no migration.** If the copy cannot be made, for example because
  the disk is full, the open fails with a typed error and the vault is left
  untouched at its old version. Migrating without the copy is the one failure
  that cannot be undone.
- The copy is kept until the **next** successful open at the new version, and
  removed then. That second open is the first one to prove the migrated vault
  is usable.
- §Failure recovery's "Restore from backup" option restores this copy. The user
  does not need an external backup.

*Today:* built. `Db::open` copies the vault with `VACUUM INTO`, which writes
the copy under the connection's own SQLCipher key, to `Db::backup_path`. The
copy is written under a temporary name, fsynced and renamed into place, and
the directory is fsynced. A copy that cannot be written fails the open with
`DbError::Backup`. The next open at the current version deletes every
`<vault>.pre-v<N>.bak` beside the vault. An in-memory vault is not copied.
Nothing in the apps offers the restore yet
([#441](https://github.com/justin13888/Sunrise/issues/441)).

### 3. Integrity is checked on open

- `PRAGMA quick_check` MUST run on every open, before any migration, and again
  after a migration batch commits.
- A failure is reported as a typed storage-integrity error, not as a generic
  SQLite error. The app offers the rebuild in rule 4. When a backup from rule 2
  exists, it also offers to restore it.
- A failed check MUST NOT start a migration. Migrating a damaged file turns a
  recoverable page into an unrecoverable schema.

*Today:* built, with one addition. A page whose SQLCipher HMAC no longer
verifies is one SQLite cannot read at all, and `quick_check` meets it as a
bare `SQLITE_ERROR`. When that happens on a file, the open runs SQLCipher's
`PRAGMA cipher_integrity_check`, which reads every page and names each bad
one. A `quick_check` finding, an `SQLITE_CORRUPT` error, or a page the HMAC
check names all return `DbError::IntegrityCheckFailed`. A wrong key
(`SQLITE_NOTADB`) stays a plain `DbError::Sqlite`, because it is not damage. Both errors map to
`ErrorCode::FatalInternal` on the wire, and no app offers the rebuild or the
restore yet ([#441](https://github.com/justin13888/Sunrise/issues/441)).

### 4. The projection can be rebuilt from the local op log

Every table in the local schema is classified, next to its `CREATE TABLE`, as
one of two kinds:

- **Source.** The table is never cleared. This covers `ops` (including parked
  ops, per [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md)
  §4), key material, identity, and local-only state such as relay intents and
  settings.
- **Projection.** The table is fully derivable from the source tables. This
  covers entity rows, per-field merge state
  ([ADR-0044](../11-adr/0044-per-field-ops.md)), control-op registers that are
  folds, FTS, and indexes.

A test fails if any table is unclassified.

**The rebuild** runs in one transaction, under a rule-2 backup:

1. Clear every projection table.
2. Replay `ops` in canonical `(hlc, device_id, seq)` order through the ordinary
   apply path.
3. Retry parked ops.
4. Rebuild FTS.

Under ADR-0044 the result does not depend on order. The canonical order keeps
side tables deterministic. A test asserts the rebuilt projection is
byte-identical to the live one for a vault built by random command sequences.

**Where it is exposed.** The rebuild is exposed over UniFFI. Account recovery
from the relay (`crates/sunrise-client-core/src/recovery.rs#recover_account`) is
exposed next to it. The macOS app has a path to each. The rebuild is offered on a
rule-3 failure and on a migration failure. It runs automatically, in the same
open, after any migration that changes materialization semantics. Adopting
per-field ops is the first such migration.

*Today:* the rebuild ships as `Engine::rebuild_projection` and
`Core::rebuild_projection` (`crates/sunrise-core/src/engine/rebuild.rs`). It
is exposed over UniFFI as `SunriseCore::rebuild_projection` and in the CLI as
`sunrise rebuild`. It differs from the design above in five ways:

- **The classification is a list in code, not a comment beside each
  `CREATE TABLE`.** `TABLE_CLASSES` in `rebuild.rs` names every table, and
  `every_table_in_the_schema_is_classified` fails when a migration adds a
  table it does not name, or when the registry projects an entity to a table
  it does not call a projection. The migration files are not edited.
- **Only entity state is a projection.** The rebuild clears the entity rows,
  their side tables, both full-text indexes and the per-field merge
  state, and refolds every applied entity op. The control-op folds
  (revocations, read bounds, features, key-envelope recipients) are classified
  as source and kept, because refolding a control op re-absorbs keys and
  re-emits envelopes.
- **Compacted ops are carried, not replayed.** An op below a
  `compaction_floor` has been deleted
  ([ADR-0059](../11-adr/0059-client-op-log-compaction.md)), so the rebuild
  first dumps the merge-state rows those ops wrote and joins them back before
  refolding the log.
- **Parked ops are not retried by the rebuild.** This build cannot read them,
  so the live projection never held them; `Engine::replay_parked_ops` retries
  them on the next open by a different build, as before.
- **No rule-2 backup is taken.** The rebuild is one transaction, so a failure
  leaves the vault as it was. A copy taken before it, and the app paths that
  offer it on a failed open, are still
  [#441](https://github.com/justin13888/Sunrise/issues/441).

Four things inside the cleared tables are kept, because no op still in the log
derives them: a placeholder `streams` row no op has stamped (the lazily seeded
Inbox among them), each routine's `materialized_until_ms`, the
`entity_versions` rows of compacted ops, which compaction keeps
([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) D10) and whose
frontiers and heads the rebuild recomputes before it joins the base, and the
`register_losers` rows compacted ops wrote. The base is a `doc_state` v1,
which carries each register's winner and not its losers, so those losers are
kept as the fold left them and the base and the refold fold in beside them
(ADR-0065 §8 moves them into the snapshot with its sixth pull request).

**It runs automatically at open when the fold changed.** A rebuild records the
version of the fold it ran, `FOLD_V`, in `projection_rebuild`, a source table
migration 0039 adds empty. `Core::open` calls `Engine::rebuild_if_stale`
after the base epochs are ensured and before parked ops are replayed or
routines materialized, and it rebuilds when that row is missing or names
another `FOLD_V`. So the first open after migration 0039 rebuilds once,
under the rule-2 backup that migration's open took, and so does the first
open of any later build whose fold differs; a `STORAGE_V` bump alone does
not. Migration 0039 relies on it: the version DAG's tables start empty, and
the rows an older build wrote without folding (before migration 0033, or
before ADR-0065's third pull request) are folded from their ops rather than
read back from the row. `FOLD_V` 2 (ADR-0065's fifth pull request) folds
registers by their conflict classes and keeps the losers in
`register_losers`, which a vault folded at 1 holds none of, so a vault opened
by that build rebuilds once more, with no migration: the tables are 0039's.

How it behaves on a large vault, decided for ADR-0065's fourth pull request:

- **One transaction, in the open, before the vault is reachable.** The vault
  shows the projection it had or the rebuilt one, never a mix. A rebuild in
  chunks would commit a half-rebuilt projection that a later chunk's failure
  or a killed process leaves behind, and one in the background would let
  reads and folds meet a half-rebuilt projection.
- **Progress is logged**, as `core.rebuild.progress` every 5,000 ops, after
  `core.rebuild.stale` names the fold version it replaces. Nothing reaches a
  client, since no subscriber exists before `Core::open` returns. Decided for
  ADR-0065's fifth pull request, with the second rebuild a vault takes: no
  progress sink in `CoreConfig` yet. One would be an additive callback a
  client registers before `Core::open`, and a client can only use it with a
  launch screen that shows a rebuild, which no client has. It is worth adding
  when the rebuild bench or a field report shows an open blocking long
  enough to need one, and adding it then breaks no caller.
- **A failure is not fatal.** The transaction rolls back, `core.rebuild.on_open_failed`
  is logged, the vault opens on the projection it had, and the next open
  tries again.
- **Cost.** A rebuild decrypts and refolds every entity op once per fold
  change; `crates/sunrise-bench/benches/rebuild.rs` measures it. Should that
  prove too slow on a device, the change to make is to the rebuild's own
  cost, for instance by deriving an entity whose history is legacy only from
  its envelope headers without decrypting, and not to its atomicity.

`rebuild_projection_reproduces_every_projection_table` rebuilds a vault
written by every command family on two replicas and compares every projection
table byte for byte. The convergence tests, `convergence_under_random_interleaving`
and `any_delivery_order_converges_and_loses_no_concurrent_write` among them,
also rebuild each replica they converge and compare
(`Engine::assert_rebuild_identity`), writers and receivers alike. Every engine
test also runs the check after each command and each delivery: an engine the
tests build turns it on for its thread, and a test that writes rows no op wrote,
as a vault older than the merge holds them, turns it off. The check compares
every projection table whole, the merge state included, since every local
write goes through the fold ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)
§5) and the merge keeps an OR-set remove whatever order its add's pruning
arrived in.

Account recovery is exposed over UniFFI as `SunriseCore::recover_account` and
reached from the CLI and the Apple onboarding. It recovers an account from the
relay into an empty vault. It is not a local rebuild.

### 5. Additive and destructive changes

- **Additive changes are always allowed.** These are `ADD COLUMN`,
  `CREATE TABLE`, `CREATE INDEX`, and a backfill that writes only new columns.
- **Destructive changes are allowed only on projection data**, and only when
  the same batch, or the rebuild that follows it, re-derives what was
  destroyed. A destructive change is a `DROP`, a narrowing type change, or a
  row rewrite or delete. Source data MUST NOT be dropped or rewritten by a
  migration.
  - Migration 0017 is the precedent: it dropped pre-hierarchy `stream_keys`
    rows only because `Keychain::open` re-derives the legacy keys it needs.
- **Old columns MAY remain indefinitely.** A column that nothing reads any more
  costs a few bytes per row. A column dropped too early costs a value nobody
  can get back. When in doubt, stop reading it and leave it in place.
- **Never drop storage for data that any client version could still write.**
  A field some build in the field can still put into an op must stay stored
  somewhere, either in its column or in the entity's `extra`. That holds even
  after this build stops modelling the field. Values that arrive from other
  devices are never the migration's to discard.
  - Example: removing `Routine.skip_dates` from the registry does not license
    dropping the data it holds. Those values move into `extra`, or the column
    stays.
- **Downgrades stay unsupported.** A newer `STORAGE_V` is refused by an older
  binary (`STORAGE_V_TOO_NEW`). Because of rule 2, the user still holds the
  pre-migration copy, and the relay still holds every op.

## Doc-schema migrations

These are coordinated:

1. **Add a field.** Trivial; old clients don't see it. Bump `doc_schema_version`.
2. **Remove a field.** Two-stage:
   - Stage A: stop reading. Bump version A.
   - Stage B: stop writing. Bump version B (after the A-version is the minimum across all known devices for the user).
3. **Rename a field.** Add new + dual-write + stop reading old + stop writing old. Or keep both forever.
4. **Change a type.** Treated as remove + add. Rare; only with extreme care.

The user's vault stores the highest `doc_schema_version` it has seen any op produced under. Devices on older versions will refuse to *originate* ops under a newer version — they read but don't write past their max.

This produces a property: a device writing schema 1 and a device writing schema 2 coexist, with the older one gracefully degrading.

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

> **Built, and not yet run automatically.** `Engine::rebuild_projection` clears
> the entity projection and the merge state and refolds the op log in one
> transaction, as §Migration rigor rule 4's *Today* note describes. A user or
> a script runs it with `sunrise rebuild`, and an app through
> `SunriseCore::rebuild_projection`. Nothing runs it on first launch after an
> upgrade yet, and no app offers it on a failed open
> ([#441](https://github.com/justin13888/Sunrise/issues/441)); for a projection
> the integrity check refuses to open, the answer is still §Failure recovery's
> option 3, "reset and resync".

The design of record for the rebuild is §Migration rigor rule 4, which
supersedes the outline below where they differ (canonical order, parked ops,
source/projection classification).

If a migration is irrecoverable (a corrupt index, a bug), the user can trigger "rebuild from log":

1. Wipe materialized tables.
2. Re-apply ops in causal order.
3. Rebuild FTS index.

This is an internal capability used by the app on first launch after an upgrade that changes materialization logic.

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

> **Amended by [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md).**
> An op this build cannot decode is no longer "reported as an invalid remote
> op". It is **parked**: stored in `ops`, kept forever, and replayed through
> the full apply path after an upgrade. A new op family also carries a feature
> id in `vault_requires`, so an older build goes read-only for that scope
> rather than writing around data it cannot see. The op log is still never
> migrated. Parking is what makes that safe. *Today:* parking is built for an
> unknown op kind (migration `0031_parked_ops.sql`,
> [#320](https://github.com/justin13888/Sunrise/issues/320)); the feature id
> is not ([#324](https://github.com/justin13888/Sunrise/issues/324)).

[ADR-0024](../11-adr/0024-key-hierarchy.md) has landed three of them —
`key_envelope`, `device_revoke` and `device_cert`, at `DOC_SCHEMA_V = 5` — and
[ADR-0025](../11-adr/0025-integration-account-entity.md) (the
`IntegrationAccount` family) will add another, so both are breaking changes to
the op vocabulary. The first three shipped on a pre-release licence that
[ADR-0042](../11-adr/0042-v0-forever.md) withdraws. No such licence exists any
more, and no flag day is available: **a migration, and any new op family, MUST
keep every older build able to merge the vault**, by parking what it cannot
read and going read-only for a scope it lacks the feature for (the amendment
above), never by assuming that no older build exists.

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
  `STORAGE_V` constant, so the list and the constant cannot drift apart;
- an upgrading open leaves an encrypted copy at the old version, the next open
  deletes it, and a copy that cannot be written stops the migration;
- a damaged page or a row that breaks its schema fails the open with
  `IntegrityCheckFailed`.

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
- **A missing fixture is written by `mise run storage-fixtures`**, which runs
  an `#[ignore]`d test so an ordinary `cargo test` reads the committed files
  rather than writing them. It writes only the fixtures that are missing and
  never rewrites a committed one: a fixture rewritten at today's schema is no
  longer old, and the chain it exists to exercise then runs over nothing. To
  rewrite one deliberately, delete it first. A test asserts every file is
  still stamped at the version its name says.
- **Every version has one**, per §Migration rigor rule 1. The v13 and v17
  files have curated seeds, because five migrations move data rather than only
  schema — 0014's `sort_order` backfill, 0017's `stream_keys` drop /
  `device_revocations` carry / Inbox re-point, 0019's `minted_by_device_id`
  backfill and `id_d_priv_wrapped` blanking, 0022's `genesis_identity_id`
  backfill and 0023's `genesis_id_s_pub` backfill — and the v13 fixture
  reaches the first two but structurally cannot reach the last three, because
  `identity` does not exist before 0017 and 0017 creates it empty. Every other
  version is filled from one list, `src/vault_fixtures/seed.rs`, that puts a
  row in every table the version has. A test fails when a migration adds a
  table the list does not fill.
- **Every row is checked, not only the curated ones.** The survival test reads
  every row of every table of a fixture before the open and requires each one
  afterwards with every value it had. The only differences it allows are the
  four deliberate ones in `DELIBERATE_CHANGES`: 0017's `stream_keys` drop, its
  `devices.revoked_at_ms` clear and its `tasks.stream_id` re-point, and 0019's
  `identity.id_d_priv_wrapped` blanking. A new migration that changes existing
  data on purpose adds its entry there, beside a test of its own that pins the
  new values.

What is still **specified and not implemented**:

- The rebuild-equivalence half of §Migration rigor rule 1, which needs the
  rebuild ([#441](https://github.com/justin13888/Sunrise/issues/441)).
- A "fuzz" job randomly orders migrations against random data states. Nothing
  like it exists, and it is a doubtful fit — the runner applies ids in ascending
  order and a random order is not a state any vault can reach.
