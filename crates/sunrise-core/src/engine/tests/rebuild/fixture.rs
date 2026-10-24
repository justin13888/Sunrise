//! Rebuild equivalence over committed vaults (`docs/04-storage/migrations.md`
//! §Migration rigor rule 1, issue #441): a vault written by the engine,
//! committed, migrated by the ordinary `Db::open`, and rebuilt from its op
//! log, must project exactly what the migration left.
//!
//! # Why not `sunrise-storage`'s fixtures
//!
//! Those are seeded row by row, with invented envelopes no keychain opens,
//! and they have to be: the storage crate cannot build an op. So nothing in
//! them can be refolded, and a rebuild over one clears their invented rows.
//! The vaults here are written through the engine, by [`super::rich_history`],
//! so every row of their projection is the fold of an op in their log.
//!
//! # What the check proves, and when
//!
//! `projection_vault_storage_v<N>.db` was written when `STORAGE_V` was `N`.
//! Opened by a build whose `STORAGE_V` is `N`, the check is the rebuild
//! identity every engine test already holds, over a file a different process
//! wrote. Opened by a later build, `Db::open` first runs every migration
//! after `N` over it, and the check is then rule 1's: a migration that
//! changes projection data in a way the fold of the log would not is a
//! failing test here, whichever side is wrong. A migration that is meant to
//! change what the fold produces comes with a `FOLD_V` bump, and the open's
//! rebuild (`Engine::rebuild_if_stale`) is what makes the two agree.
//!
//! # Writing one
//!
//! `mise run storage-fixtures` also runs
//! [`write_the_missing_projection_fixture`], which writes the fixture for this
//! build's `STORAGE_V` when there is none and never rewrites one. A new one
//! is worth committing when a migration changes the shape of projection data:
//! the fixture before it is then the one that exercises it.
//!
//! The keys are the engine tests' own (`Keychain::for_test_seeded`, derived
//! from [`ROOT`]), not stored in the file: a change to how that test keychain
//! derives them is a regenerated fixture, not a bug.

use std::fs;
use std::path::{Path, PathBuf};

use super::super::testutil::*;
use super::super::*;
use super::rich_history;
use sunrise_cbor::version::STORAGE_V;

/// The prefix of every committed projection fixture's file name.
const PREFIX: &str = "projection_vault_storage_v";

/// The device seed the fixture's ops are signed under.
const SEED: [u8; 32] = [0x2d; 32];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Every committed projection fixture, with the `STORAGE_V` it was written
/// at, oldest first.
fn committed() -> Vec<(u32, PathBuf)> {
    let mut found: Vec<(u32, PathBuf)> = fs::read_dir(fixture_dir())
        .expect("the fixture directory")
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let v = name
                .strip_prefix(PREFIX)?
                .strip_suffix(".db")?
                .parse()
                .ok()?;
            Some((v, path))
        })
        .collect();
    found.sort();
    found
}

fn engine_for_fixture() -> Engine {
    engine_seeded(ROOT, SEED, Arc::new(FakeClock(PLMutex::new(NOW as u64))))
}

/// Write the fixture for this build's `STORAGE_V` if it is missing.
///
/// `#[ignore]`d: an ordinary `cargo test` reads the committed files. Run it
/// through `mise run storage-fixtures`.
#[test]
#[ignore = "writes a committed vault fixture; run it through `mise run storage-fixtures`"]
fn write_the_missing_projection_fixture() {
    let path = fixture_dir().join(format!("{PREFIX}{STORAGE_V}.db"));
    if path.exists() {
        return;
    }
    let mut db = Db::open(&path, &VaultRootKey::from_bytes(ROOT)).expect("create the fixture");
    let e = engine_for_fixture();
    rich_history(&e, &mut db);
    // What the first open of a vault records, so that a later open finds the
    // projection current and the check compares what the migration left,
    // not a rebuild of it.
    e.rebuild_projection(&mut db).expect("record the fold");
    db.conn()
        .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    drop(db);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(suffix);
        let _ = fs::remove_file(sidecar);
    }
}

/// There is one to check: an empty directory would pass the check below.
#[test]
fn a_projection_fixture_is_committed() {
    let found = committed();
    assert!(
        !found.is_empty(),
        "no {PREFIX}*.db under {}; run `mise run storage-fixtures`",
        fixture_dir().display()
    );
    for (v, path) in &found {
        assert!(
            *v <= u32::from(STORAGE_V),
            "{} is from a later build than this one",
            path.display()
        );
    }
}

/// Rule 1's rebuild equivalence, over every committed fixture: migrated by
/// the ordinary open, then rebuilt, every projection table reads as the
/// migration left it.
#[test]
fn every_projection_fixture_rebuilds_to_what_its_migration_left() {
    for (v, source) in committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        fs::copy(&source, &path).unwrap();
        let mut db = Db::open(&path, &VaultRootKey::from_bytes(ROOT))
            .unwrap_or_else(|e| panic!("the v{v} fixture must open and migrate: {e}"));
        let tasks: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert!(tasks > 0, "the v{v} fixture projects nothing to compare");
        let e = engine_for_fixture();
        // A fixture written at another fold is rebuilt by the open first, as
        // a vault in the field would be. At this build's fold nothing runs,
        // and the identity below compares the migrated rows themselves.
        e.rebuild_if_stale(&mut db).unwrap();
        e.assert_rebuild_identity(&db);
        let report = e.rebuild_projection(&mut db).unwrap();
        assert_eq!(report.ops_skipped, 0, "v{v}: {report:?}");
        assert!(report.ops_replayed > 20, "v{v}: {report:?}");
    }
}
