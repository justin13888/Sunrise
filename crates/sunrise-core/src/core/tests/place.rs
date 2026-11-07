//! Presence at a Place is held in memory and written nowhere (ADR-0051 §3).

use super::*;
use sunrise_domain::{PlaceEval, PlaceId};
use sunrise_id::Ulid;

fn place(n: u8) -> PlaceId {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    PlaceId::from_ulid(Ulid::from_bytes(bytes))
}

/// The row count of every table in the vault database, by name: the op log,
/// the outbox and every projection and device-local table alike.
fn every_table(core: &Core) -> Vec<(String, i64)> {
    let db = core.db.lock();
    let conn = db.conn();
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names.retain(|n| !n.starts_with("sqlite_"));
    names
        .into_iter()
        .map(|n| {
            let count = conn
                .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                .unwrap();
            (n, count)
        })
        .collect()
}

#[tokio::test]
async fn presence_is_never_written_to_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut events = core.changes();
    let before = every_table(&core);

    core.on_place_presence(place(1), true);
    core.on_place_presence(place(2), false);
    core.on_place_presence(place(1), false);
    core.on_place_presence(place(1), true);
    core.forget_place_presence(place(2));

    assert_eq!(
        every_table(&core),
        before,
        "a presence report must add no op, no outbox entry and no row anywhere"
    );
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a presence report is not a change to the vault"
    );
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Satisfied);
    assert_eq!(
        core.evaluate_place(&[place(2)]),
        PlaceEval::Unknown,
        "a forgotten place is unknown again"
    );
    core.close().await.unwrap();
}

#[tokio::test]
async fn a_reopened_vault_knows_no_presence() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    core.on_place_presence(place(1), true);
    core.on_place_presence(place(2), false);
    assert_eq!(
        core.evaluate_place(&[place(2), place(1)]),
        PlaceEval::Satisfied
    );
    assert_eq!(core.evaluate_place(&[place(2)]), PlaceEval::Violated);
    core.close().await.unwrap();

    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    for p in [place(1), place(2)] {
        assert_eq!(
            core.evaluate_place(&[p]),
            PlaceEval::Unknown,
            "with no OS report since open, every place is unknown"
        );
    }
    core.close().await.unwrap();
}

#[tokio::test]
async fn revoking_permission_or_closing_leaves_every_place_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    core.on_place_presence(place(1), false);
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Violated);
    core.clear_place_presence();
    assert_eq!(
        core.evaluate_place(&[place(1)]),
        PlaceEval::Unknown,
        "a denied permission is unknown, never a violation"
    );

    core.on_place_presence(place(1), true);
    core.shutdown().await;
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Unknown);
    core.on_place_presence(place(1), true);
    assert_eq!(
        core.evaluate_place(&[place(1)]),
        PlaceEval::Unknown,
        "a report after close is dropped"
    );
}
