//! The Postgres backends against a real Postgres, where one is provided.
//!
//! Every test here needs `SUNRISE_TEST_POSTGRES_URL` (ADR-0062 §1): CI's
//! `postgres` job provides one, and a developer runs one in Docker
//! (`docs/06-server/self-hosting.md` §Several nodes over Postgres). Without it
//! each test prints why it did nothing and passes, as the S3 suite will.
//!
//! Each test gets a schema of its own and a notification channel of its own,
//! so tests share one database without sharing a row or a notice.

use std::sync::Arc;

use super::{OpenError, PgConnect, PgLimiter, PgSessions, PgStore};
use crate::auth::Subject;
use crate::store::MetadataStore;
use crate::sync_session::{Session, SessionBackend};

/// The variable that names the test database.
pub(super) const ENV: &str = "SUNRISE_TEST_POSTGRES_URL";

/// The test database's URL, or `None` with the reason printed.
pub(super) fn url() -> Option<String> {
    let url = std::env::var(ENV).ok().filter(|u| !u.trim().is_empty());
    if url.is_none() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("skipped: {ENV} is not set, so there is no Postgres to run against");
        }
    }
    url
}

/// A connection confined to a fresh schema of its own.
pub(super) async fn isolated(url: &str) -> PgConnect {
    let mut raw = [0u8; 8];
    getrandom::getrandom(&mut raw).unwrap();
    let schema = format!("t{}", hex::encode(raw));
    let (client, conn) = url
        .parse::<tokio_postgres::Config>()
        .unwrap()
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    PgConnect::from_url(url, None).unwrap().isolated(&schema)
}

/// A migrated store in a fresh schema.
pub(super) async fn store(url: &str) -> PgStore {
    PgStore::open(isolated(url).await).await.unwrap()
}

fn session(deadline_ms: Option<u64>, seen_ms: u64) -> Session {
    Session {
        account: [1u8; 16],
        account_id: "acc".to_owned(),
        subject: Subject::new("iss", "sub"),
        device_id: Some("dev".to_owned()),
        deadline_ms,
        conn: u64::MAX - 7,
        negotiated: sunrise_wire_protocol::HelloAck {
            server_app_v: "0".to_owned(),
            wire_proto: 1,
            crypto_suite: 1,
            doc_schema_floor: 1,
            capabilities: 0,
            server_time_ms: 0,
        },
        streams: vec![sunrise_wire_protocol::SubscribeEntry {
            cursors: vec![sunrise_wire_protocol::CursorEntry {
                device_id: [9; 16],
                last_applied_seq: 42,
            }],
            stream_id: [0x11; 16],
        }],
        subscribe_unserved: false,
        seen_ms,
    }
}

/// The metadata conformance suite every backend owes.
#[tokio::test]
async fn the_postgres_store_passes_the_metadata_conformance_suite() {
    let Some(url) = url() else { return };
    crate::store::conformance::run(&store(&url).await).await;
}

/// The session conformance suite, and a session whose every field survives
/// the row: the streams, the cursors and a connection id with its top bit
/// set.
#[tokio::test]
async fn postgres_sessions_pass_the_session_conformance_suite() {
    let Some(url) = url() else { return };
    let sessions = PgSessions::new(store(&url).await);
    crate::sync_session::conformance::run(&sessions, session).await;

    let stored = session(None, 3);
    sessions
        .insert("ses_x".into(), stored.clone())
        .await
        .unwrap();
    let read = sessions.get("ses_x", 3).await.unwrap().unwrap();
    assert_eq!(read.conn, stored.conn);
    assert_eq!(read.streams, stored.streams);
    assert_eq!(read.device_id, stored.device_id);
    assert_eq!(read.subject, stored.subject);
}

/// The row is keyed by the id's hash: nothing in the table is an id anyone
/// could present.
#[tokio::test]
async fn a_session_row_holds_no_usable_id() {
    let Some(url) = url() else { return };
    let store = store(&url).await;
    let sessions = PgSessions::new(store.clone());
    let id = "ses_00112233445566778899aabbccddeeff";
    sessions.insert(id.into(), session(None, 0)).await.unwrap();
    let row = store
        .client()
        .await
        .unwrap()
        .query_one("SELECT session_h, body FROM sync_sessions", &[])
        .await
        .unwrap();
    let key: Vec<u8> = row.get(0);
    assert_eq!(key, blake3::hash(id.as_bytes()).as_bytes().to_vec());
    let body: String = row.get(1);
    assert!(!body.contains(id), "{body}");
}

/// The limiter conformance suite: the same decisions as the in-process one.
#[tokio::test]
async fn the_postgres_limiter_passes_the_limiter_conformance_suite() {
    let Some(url) = url() else { return };
    let limiter = PgLimiter::new(store(&url).await);
    crate::api::ratelimit::store::conformance::run(&limiter).await;
}

/// Two nodes charging one key at once spend one budget: the decision and the
/// charge are one statement.
#[tokio::test]
async fn two_nodes_charging_one_key_share_its_budget() {
    use crate::api::ratelimit::bucket::Quota;
    use crate::api::ratelimit::store::LimiterStore as _;
    let Some(url) = url() else { return };
    let connect = isolated(&url).await;
    let a = PgLimiter::new(PgStore::open(connect.clone()).await.unwrap());
    let b = PgLimiter::new(PgStore::open(connect).await.unwrap());
    let q = Quota::per_minute(20);
    let mut charges = Vec::new();
    for i in 0..40 {
        let node = if i % 2 == 0 { a.clone() } else { b.clone() };
        charges.push(tokio::spawn(async move {
            node.take("shared", q, 1, 1_704_067_200_000).await.allowed
        }));
    }
    let mut admitted = 0;
    for c in charges {
        admitted += usize::from(c.await.unwrap());
    }
    assert_eq!(
        admitted, 20,
        "one bucket of 20, however the charges interleave"
    );
}

/// Opening an already migrated database changes nothing, and one a newer
/// release migrated is refused before anything is written.
#[tokio::test]
async fn a_newer_database_is_refused_and_a_current_one_reopens() {
    let Some(url) = url() else { return };
    let connect = isolated(&url).await;
    let store = PgStore::open(connect.clone()).await.unwrap();
    PgStore::open(connect.clone()).await.unwrap();
    super::migrations::stamp(
        &store.client().await.unwrap(),
        super::migrations::LATEST + 1,
    )
    .await;
    let refused = PgStore::open(connect).await.unwrap_err();
    assert!(
        matches!(refused, OpenError::SchemaTooNew { found, .. } if found == i64::from(super::migrations::LATEST) + 1),
        "{refused}"
    );
}

/// Two nodes starting together migrate the database once between them.
#[tokio::test]
async fn nodes_starting_together_migrate_once() {
    let Some(url) = url() else { return };
    let connect = isolated(&url).await;
    let opens: Vec<_> = (0..4)
        .map(|_| tokio::spawn(PgStore::open(connect.clone())))
        .collect();
    for open in opens {
        open.await.unwrap().unwrap();
    }
}

/// One node leads maintenance at a time, and its lead ends with its guard,
/// as it would with its connection.
#[tokio::test]
async fn one_node_leads_maintenance_at_a_time() {
    let Some(url) = url() else { return };
    let connect = isolated(&url).await;
    let a = PgStore::open(connect.clone()).await.unwrap();
    let b = PgStore::open(connect).await.unwrap();
    let lead = MetadataStore::lead_maintenance(&a)
        .await
        .unwrap()
        .expect("the first node leads");
    assert!(
        MetadataStore::lead_maintenance(&b).await.unwrap().is_none(),
        "the second skips the pass"
    );
    drop(lead);
    // The lock goes with the connection, which closes as its task is
    // aborted; the next attempt finds it free.
    let mut led = None;
    for _ in 0..50 {
        led = MetadataStore::lead_maintenance(&b).await.unwrap();
        if led.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(led.is_some(), "a released lead is taken by the next node");
}

/// The URL file holds a password, so only its owner may read it; a URL that
/// asks for TLS with no CA to verify against is refused rather than
/// downgraded. Neither needs a database.
#[test]
fn the_url_file_and_its_tls_are_checked_before_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pg.url");
    std::fs::write(&file, "postgres://u:secret@db.example/sunrise\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            PgConnect::from_files(&file, None),
            Err(OpenError::UrlPermissions { mode: 0o644, .. })
        ));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(PgConnect::from_files(&file, None).is_ok());

    std::fs::write(&file, "not a url at all ===\n").unwrap();
    let e = PgConnect::from_files(&file, None).unwrap_err().to_string();
    assert!(
        !e.contains("not a url"),
        "the file's content is not echoed: {e}"
    );

    assert!(matches!(
        PgConnect::from_url("postgres://u@db.example/s?sslmode=require", None),
        Err(OpenError::TlsWithoutCa)
    ));
    let shown = format!(
        "{:?}",
        PgConnect::from_url("postgres://u:secret@db.example/s", None).unwrap()
    );
    assert!(!shown.contains("secret"), "{shown}");
}

/// A state built for Postgres is fed by the bus and serves its sessions from
/// the shared table.
#[tokio::test]
async fn a_postgres_state_is_fed_by_the_bus() {
    let Some(url) = url() else { return };
    let (state, feed) = crate::state::ServerState::on_postgres(
        crate::ServerConfig::default(),
        store(&url).await,
        Arc::new(crate::state::SystemClock),
    )
    .unwrap();
    assert!(state.relay.fed());
    assert!(state.store.as_sqlite().is_none());
    feed.shutdown();
}

/// Every transaction defaults to READ COMMITTED, whatever the URL (and so,
/// a fortiori, the role or the server) asked for: the read-then-write
/// transactions are written for it.
#[tokio::test]
async fn transactions_are_read_committed_whatever_the_default() {
    let Some(url) = url() else { return };
    let sep = if url.contains('?') { '&' } else { '?' };
    let asked = format!("{url}{sep}options=-c%20default_transaction_isolation%3Dserializable");
    let store = store(&asked).await;
    let level: String = store
        .client()
        .await
        .unwrap()
        .query_one("SHOW default_transaction_isolation", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(level, "read committed");
}

/// A read that finds an expired session deletes it only while it is still
/// expired: a refresh another node committed since the read keeps its row.
#[tokio::test]
async fn reading_an_expired_session_spares_one_refreshed_since() {
    let Some(url) = url() else { return };
    let store = store(&url).await;
    let sessions = PgSessions::new(store.clone());
    sessions
        .insert("ses_r".into(), session(Some(100), 0))
        .await
        .unwrap();
    // The deadline column as a concurrent refresh leaves it, under a body the
    // read still decodes as expired.
    store
        .client()
        .await
        .unwrap()
        .execute("UPDATE sync_sessions SET deadline_ms = 10000", &[])
        .await
        .unwrap();
    assert!(sessions.get("ses_r", 200).await.unwrap().is_none());
    assert_eq!(
        sessions.count().await.unwrap(),
        1,
        "the refreshed row stays"
    );

    sessions
        .insert("ses_e".into(), session(Some(100), 0))
        .await
        .unwrap();
    assert!(sessions.get("ses_e", 200).await.unwrap().is_none());
    assert_eq!(sessions.count().await.unwrap(), 1, "the expired row went");
}

/// Erasing an account while its channels are being appended to completes on
/// both sides: the erasure holds the account lock every append shares, first.
#[tokio::test]
async fn erasing_an_account_beside_its_appends_does_not_deadlock() {
    let Some(url) = url() else { return };
    let store = store(&url).await;
    let account_h = crate::account_ns::account_key("acc-erase");
    let caps = crate::relay_log::DurableCaps {
        max_bytes: 64,
        max_age_ms: u64::MAX,
    };
    for round in 0..20u64 {
        let mut appends = Vec::new();
        for stream in 0..3u8 {
            let store = store.clone();
            appends.push(tokio::spawn(async move {
                for n in 0..5u64 {
                    let head = [crate::relay::FrameHead {
                        device_id: [7; 16],
                        max_seq: round * 10 + n + 1,
                    }];
                    let ops_h = [stream ^ u8::try_from(n).unwrap(); 32];
                    store
                        .relay_append(
                            (account_h, [stream; 16]),
                            &[0; 40],
                            &head,
                            1,
                            Some(&ops_h),
                            n,
                            1,
                            caps,
                        )
                        .await
                        .unwrap();
                }
            }));
        }
        for _ in 0..5 {
            store.erase_account("acc-erase").await.unwrap();
        }
        for a in appends {
            a.await.unwrap();
        }
    }
}
