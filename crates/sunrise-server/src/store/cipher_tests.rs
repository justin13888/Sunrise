//! The relay database at rest: encryption, its migration, the online backup
//! and a restore from it, each against a real file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::types::Value;
use rusqlite::Connection;

use super::*;
use crate::auth::Subject;
use crate::relay::{FrameHead, StreamKey};
use crate::relay_log::DurableCaps;
use crate::store::{NewDevice, Store, DEFAULT_BUSY_TIMEOUT};

const NOW: u64 = 1_800_000_000_000;
const CHANNEL: StreamKey = ([0xa1; 16], [0x11; 16]);
const DEV: [u8; 16] = [0x22; 16];

fn key(byte: u8) -> DbKey {
    DbKey::from_bytes([byte; 32])
}

fn open(path: &Path, key: Option<&DbKey>) -> Result<Store, StoreError> {
    Store::open_keyed(Some(path), DEFAULT_BUSY_TIMEOUT, key)
}

fn unbounded() -> DurableCaps {
    DurableCaps {
        max_bytes: u64::MAX,
        max_age_ms: u64::MAX,
    }
}

/// Append the frame for `seq` to [`CHANNEL`].
fn append(store: &Store, seq: u64) {
    store
        .relay_append(
            CHANNEL,
            &seq.to_be_bytes(),
            &[FrameHead {
                device_id: DEV,
                max_seq: seq,
            }],
            None,
            seq,
            NOW + seq,
            unbounded(),
        )
        .unwrap();
}

/// An account with a device and a push token, and `frames` relay frames.
fn populate(store: &Store, frames: u64) {
    let account = store
        .resolve_account(&Subject::new("https://idp.example", "alice"), true, NOW)
        .unwrap()
        .account_id;
    let device = store
        .register_device(
            &account,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: None,
                nickname: "laptop".into(),
                platform: "linux".into(),
                app_version: Some("0.1.0".into()),
            },
            NOW,
        )
        .unwrap();
    store
        .upsert_push_token(&device.device_id, "apns", "token", NOW)
        .unwrap();
    for seq in 1..=frames {
        append(store, seq);
    }
}

/// The schema version, and every row of every table by table name.
type Dump = (i64, Vec<(String, Vec<Vec<Value>>)>);

/// A database's [`Dump`].
fn dump(conn: &Connection) -> Dump {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let rows = tables
        .into_iter()
        .map(|t| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM \"{t}\" ORDER BY rowid"))
                .unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| (0..n).map(|i| r.get::<_, Value>(i)).collect())
                .unwrap()
                .collect::<Result<Vec<Vec<Value>>, _>>()
                .unwrap();
            (t, rows)
        })
        .collect();
    let version = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    (version, rows)
}

fn dump_store(store: &Store) -> Dump {
    dump(&store.conn.lock())
}

fn header(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()[..16].to_vec()
}

/// The frame ids [`CHANNEL`] holds after `after`, in order.
fn frame_ids(store: &Store, after: u64) -> (Vec<u64>, usize) {
    let (frames, gaps) = store
        .relay_replay_after(CHANNEL, after, &HashMap::new())
        .unwrap();
    (frames.into_iter().map(|(id, _)| id).collect(), gaps.len())
}

#[test]
fn an_encrypted_file_has_no_sqlite_header_and_will_not_open_without_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    {
        let s = open(&db, Some(&key(1))).unwrap();
        populate(&s, 3);
        assert!(s.is_encrypted());
    }

    assert_ne!(header(&db), PLAINTEXT_HEADER.to_vec());
    let raw = std::fs::read(&db).unwrap();
    assert!(
        !raw.windows(5).any(|w| w == b"alice") && !raw.windows(6).any(|w| w == b"laptop"),
        "no plaintext row may be readable in the file"
    );

    assert!(matches!(
        open(&db, None),
        Err(StoreError::KeyRequired { path }) if path == db
    ));
    // Nor through SQLite directly, bypassing the store's own check.
    let bare = Connection::open(&db).unwrap();
    let read = bare.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    });
    assert!(
        matches!(&read, Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::NotADatabase),
        "{read:?}"
    );

    let s = open(&db, Some(&key(1))).unwrap();
    assert_eq!(frame_ids(&s, 0).0, vec![1, 2, 3]);
}

#[test]
fn the_wrong_key_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    drop(open(&db, Some(&key(1))).unwrap());
    let before = std::fs::read(&db).unwrap();

    assert!(matches!(
        open(&db, Some(&key(2))),
        Err(StoreError::WrongKey { path }) if path == db
    ));
    assert_eq!(std::fs::read(&db).unwrap(), before);
}

/// The one-way migration: every row of every table survives, the schema
/// version with them, and the plaintext original is kept beside it.
#[test]
fn a_plaintext_database_is_encrypted_with_every_row_and_its_original_kept() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let before = {
        let s = open(&db, None).unwrap();
        populate(&s, 5);
        dump_store(&s)
    };
    assert_eq!(header(&db), PLAINTEXT_HEADER.to_vec());

    let s = open(&db, Some(&key(7))).unwrap();
    assert!(s.is_encrypted());
    assert_eq!(dump_store(&s), before, "every row, and the version");
    assert!(before.1.iter().any(|(_, rows)| !rows.is_empty()));
    drop(s);
    assert_ne!(header(&db), PLAINTEXT_HEADER.to_vec());

    let kept = pre_encryption_copy(&db);
    assert_eq!(header(&kept), PLAINTEXT_HEADER.to_vec());
    assert_eq!(dump(&Connection::open(&kept).unwrap()), before);
    assert!(!sibling(&db, ENCRYPTING_SUFFIX).exists());

    // Reopening does not migrate again: the file is encrypted now.
    let kept_bytes = std::fs::read(&kept).unwrap();
    let s = open(&db, Some(&key(7))).unwrap();
    assert_eq!(dump_store(&s), before);
    assert_eq!(std::fs::read(&kept).unwrap(), kept_bytes);
}

/// Changes a relay made in WAL mode and never checkpointed are in the `-wal`
/// beside the file, not in it, and the migration must not leave them behind.
#[test]
fn the_migration_carries_writes_still_in_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let live = open(&db, None).unwrap();
    populate(&live, 2);
    // Copy the file and its WAL as a crashed relay would leave them: closing
    // the connection would checkpoint, and this test is of the case where
    // nothing did.
    let crashed = dir.path().join("crashed");
    std::fs::create_dir(&crashed).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", db.display()));
        if from.exists() {
            std::fs::copy(&from, crashed.join(format!("sunrise.db{suffix}"))).unwrap();
        }
    }
    let before = dump_store(&live);
    drop(live);

    let db = crashed.join("sunrise.db");
    let s = open(&db, Some(&key(3))).unwrap();
    assert_eq!(dump_store(&s), before);
}

/// The migration needs the file to itself: a relay still running on the
/// plaintext database would keep writing into the original.
#[test]
fn the_migration_is_refused_while_another_process_holds_the_plaintext_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let running = open(&db, None).unwrap();
    populate(&running, 1);

    let refused = Store::open_keyed(Some(&db), Duration::from_millis(50), Some(&key(1)));
    assert!(
        matches!(&refused, Err(StoreError::InUse { .. })),
        "{refused:?}"
    );
    drop(running);
    assert_eq!(header(&db), PLAINTEXT_HEADER.to_vec(), "left plaintext");
}

#[test]
fn a_key_file_is_64_hex_digits_readable_by_its_owner_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.key");
    let write = |text: &str, mode: u32| {
        // Replaced rather than rewritten, since the last mode may be 0400.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    };

    write(&format!("{}\n", "ab".repeat(32)), 0o600);
    assert_eq!(
        DbKey::from_file(&path).unwrap(),
        DbKey::from_bytes([0xab; 32])
    );
    write(&"ab".repeat(32), 0o400);
    assert!(DbKey::from_file(&path).is_ok());

    write(&"ab".repeat(31), 0o600);
    assert!(matches!(
        DbKey::from_file(&path),
        Err(StoreError::KeyFile { .. })
    ));
    write("not a key", 0o600);
    assert!(matches!(
        DbKey::from_file(&path),
        Err(StoreError::KeyFile { .. })
    ));
    assert!(matches!(
        DbKey::from_file(&dir.path().join("absent")),
        Err(StoreError::KeyFile { .. })
    ));

    #[cfg(unix)]
    {
        write(&"ab".repeat(32), 0o644);
        assert!(matches!(
            DbKey::from_file(&path),
            Err(StoreError::KeyPermissions { mode: 0o644, .. })
        ));
        write(&"ab".repeat(32), 0o640);
        assert!(matches!(
            DbKey::from_file(&path),
            Err(StoreError::KeyPermissions { mode: 0o640, .. })
        ));
    }
    assert!(!format!("{:?}", DbKey::from_bytes([0xab; 32])).contains("ab"));
}

/// **Backup under load.** A writer appends to the relay log the whole time
/// the backup runs, through its own connection as the relay would; the copy
/// must be whole, open under the same key, and hold an unbroken prefix of
/// the log.
#[test]
fn a_backup_taken_under_concurrent_appends_is_whole_keyed_and_a_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let relay = Arc::new(open(&db, Some(&key(9))).unwrap());
    populate(&relay, 0);
    // Opened before the load starts, as an operator's `admin` is: opening
    // migrates, which takes the write lock the writer below never lets go of
    // for long. The backup is what has to run under load.
    let admin = open(&db, Some(&key(9))).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let appended = Arc::new(AtomicU64::new(0));
    let writer = {
        let (relay, stop, appended) = (relay.clone(), stop.clone(), appended.clone());
        std::thread::spawn(move || {
            let mut seq = 0;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                append(&relay, seq);
                appended.store(seq, Ordering::Relaxed);
            }
        })
    };
    while appended.load(Ordering::Relaxed) < 200 {
        std::thread::yield_now();
    }

    let dest = dir.path().join("backup.db");
    admin.backup_to(&dest).unwrap();
    let at_backup = appended.load(Ordering::Relaxed);
    // The writer kept going past the copy, which is what makes the copy's
    // prefix a statement about a live database.
    while appended.load(Ordering::Relaxed) < at_backup + 50 {
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert!(matches!(
        open(&dest, None),
        Err(StoreError::KeyRequired { .. })
    ));
    let copy = open(&dest, Some(&key(9))).unwrap();
    let integrity: String = copy
        .conn
        .lock()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");

    let (ids, gaps) = frame_ids(&copy, 0);
    assert_eq!(gaps, 0);
    assert!(
        ids.len() >= 200,
        "the copy holds what was appended before it"
    );
    assert_eq!(
        ids,
        (1..=ids.len() as u64).collect::<Vec<_>>(),
        "an unbroken prefix of the log"
    );
    let (live, _) = frame_ids(&relay, 0);
    assert!(
        live.len() > ids.len(),
        "the relay was writing past the copy"
    );
    assert_eq!(&live[..ids.len()], &ids[..]);
}

/// **Restore.** Back up, lose the data dir, put the backup in its place, and
/// a client resuming from a cursor the backup covers gets every frame after
/// it with no gap.
#[test]
fn a_restored_backup_resumes_a_client_from_its_cursor_without_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let db = data.join("sunrise.db");
    let dest = dir.path().join("backup.db");
    {
        let s = open(&db, Some(&key(4))).unwrap();
        populate(&s, 10);
        s.backup_to(&dest).unwrap();
        // Written after the backup, and lost with the data dir.
        for seq in 11..=15 {
            append(&s, seq);
        }
    }

    std::fs::remove_dir_all(&data).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::copy(&dest, &db).unwrap();

    let restored = open(&db, Some(&key(4))).unwrap();
    // The client had applied through frame 4.
    assert_eq!(frame_ids(&restored, 4), ((5..=10).collect::<Vec<_>>(), 0));
    // And the relay carries on from there: the next append follows on.
    append(&restored, 11);
    assert_eq!(frame_ids(&restored, 10).0, vec![11]);
}

#[test]
fn a_backup_of_a_plaintext_database_is_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let s = open(&db, None).unwrap();
    populate(&s, 2);
    let dest = dir.path().join("backup.db");
    s.backup_to(&dest).unwrap();
    assert_eq!(header(&dest), PLAINTEXT_HEADER.to_vec());
    assert_eq!(dump_store(&open(&dest, None).unwrap()), dump_store(&s));
    assert!(
        matches!(s.backup_to(&dest), Err(StoreError::Io { .. })),
        "never over a file"
    );
}

#[test]
fn rekey_moves_the_database_to_the_new_key_and_the_old_one_stops_opening_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let before = {
        let s = open(&db, Some(&key(1))).unwrap();
        populate(&s, 3);
        s.rekey(&key(2)).unwrap();
        let journal: String = s
            .conn
            .lock()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal", "back in WAL mode after the rotation");
        dump_store(&s)
    };
    assert!(matches!(
        open(&db, Some(&key(1))),
        Err(StoreError::WrongKey { .. })
    ));
    assert_eq!(dump_store(&open(&db, Some(&key(2))).unwrap()), before);
}

#[test]
fn rekey_is_refused_while_the_relay_has_the_file_open_and_for_a_plaintext_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sunrise.db");
    let relay = open(&db, Some(&key(1))).unwrap();
    populate(&relay, 1);
    let admin = Store::open_keyed(Some(&db), Duration::from_millis(50), Some(&key(1))).unwrap();
    let refused = admin.rekey(&key(2));
    assert!(
        matches!(&refused, Err(StoreError::InUse { .. })),
        "{refused:?}"
    );
    drop((relay, admin));
    open(&db, Some(&key(1))).expect("the old key still opens it");

    let plain = dir.path().join("plain.db");
    assert!(matches!(
        open(&plain, None).unwrap().rekey(&key(2)),
        Err(StoreError::NotEncrypted)
    ));
}

/// `encrypt` and `key_file` must agree, and the key must live outside the
/// data dir, so a copy of the data dir is never also a copy of its key.
#[test]
fn the_storage_table_names_a_key_only_with_encrypt_on_and_outside_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let write_key = |path: &Path| {
        std::fs::write(path, "cd".repeat(32)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    };
    let outside = dir.path().join("db.key");
    write_key(&outside);
    let inside = data.join("db.key");
    write_key(&inside);

    assert_eq!(DbKey::for_storage(false, None, Some(&data)).unwrap(), None);
    assert_eq!(
        DbKey::for_storage(true, Some(&outside), Some(&data)).unwrap(),
        Some(DbKey::from_bytes([0xcd; 32]))
    );
    assert!(matches!(
        DbKey::for_storage(true, None, Some(&data)),
        Err(StoreError::KeyConfig(_))
    ));
    assert!(matches!(
        DbKey::for_storage(false, Some(&outside), Some(&data)),
        Err(StoreError::KeyConfig(_))
    ));
    assert!(matches!(
        DbKey::for_storage(true, Some(&inside), Some(&data)),
        Err(StoreError::KeyInDataDir { .. })
    ));
    // Through `..`, which only the canonical comparison sees through.
    let dotted = data.join("..").join("data").join("db.key");
    assert!(matches!(
        DbKey::for_storage(true, Some(&dotted), Some(&data)),
        Err(StoreError::KeyInDataDir { .. })
    ));
}
