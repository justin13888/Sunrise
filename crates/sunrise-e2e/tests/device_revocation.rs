//! Revoking a device, end to end, over a live relay.
//!
//! Three devices on one account: A invites B and C, then revokes C. Two things
//! must follow, and this test asserts both:
//!
//! - **The account keeps working.** Revocation mints a fresh epoch for every
//!   stream in the rotation set — the vault-meta stream and the Inbox included,
//!   not only user Streams — and seals each one to every device *and* to the
//!   account identity. If any of that were wrong the surviving devices would go
//!   dark, so B converging on a Stream and a Context created after the cut is
//!   the assertion that rotation and redistribution actually work.
//! - **Revocation is recorded and converged, and enforces nothing.** C is
//!   marked revoked on every replica and keeps every capability it had. That is
//!   the honest scope of this slice, and the assertions below are written to
//!   the property that survives: the rotation reaches every device, so the
//!   account keeps working across a revocation. It reaches C too.
//!
//! Both enforcement claims were built here and removed. Refusing a revoked
//! device's ops freezes the refusing replica's sync cursor for it while the
//! relay goes on accepting its uploads, and retention turns that into a
//! permanent data-loss warning on every peer; withholding its Stream key parks
//! every op behind it in that stream forever. And neither withholds anything,
//! because pairing hands every device `ID_D_priv` and each epoch is sealed to
//! the identity as well — so a revoked device opens the identity copy. Reads
//! are #76, writes are #82 behind #80, and converging the effect is #78.
//!
//! # What this test deliberately does not assert, and why
//!
//! It does not assert that C fails to *read* what A wrote after the cut,
//! because it does not. `key_envelope` ops are sealed to two recipient classes
//! (ADR-0024 decision 4): each remaining device's `D_D_pub`, and the account
//! identity's `ID_D_pub`. The identity copy is what lets a recovery with no
//! surviving device restore readable content rather than an empty vault
//! (`docs/03-crypto/recovery.md` §8). But pairing hands every device
//! `ID_D_priv` (`docs/03-crypto/pairing-and-onboarding.md` §102-103), so a
//! revoked device opens the identity-addressed envelope for every new epoch and
//! reads straight through the rotation.
//!
//! `docs/01-architecture/threat-model.md` §A3 names all three legs of the
//! mitigation — revoke, rotate Stream keys, **rotate the identity key**. This
//! slice builds the machinery for the first two and enforces neither. Until the
//! third lands, forward secrecy against a revoked device is not achieved, and
//! no test here should claim it is.
//!
//! Separately, and for the same root cause: a revoked device still holds
//! `ID_S_priv`, so it can issue itself a fresh, valid `DeviceCert` under a new
//! device id. Revocation names a device, and the identity keys are what name
//! devices.
//!
//! What rotation could never do, even complete: take back what C already had.
//! C keeps every key it held and therefore everything it could already read,
//! which is why the pre-revocation task stays readable on C throughout.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, Query, QueryResult, RevokeReason, SystemClock};
use sunrise_domain::{ContextDraft, StreamDraft, TaskDraft};
use sunrise_e2e::{
    canonical_tasks, open_paired_core, open_synced_core, spawn_relay, wait_live,
    wait_tasks_converge,
};
use sunrise_id::{EntityKind, EntityRef};

const ROOT: [u8; 32] = [0x37; 32];
const TIMEOUT: Duration = Duration::from_secs(30);

async fn create_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

async fn create_stream(core: &Core, name: &str) -> EntityRef {
    core.submit(Command::CreateStream(StreamDraft {
        name: name.into(),
        ..Default::default()
    }))
    .await
    .expect("create stream")
    .entity
}

async fn task_titles(core: &Core) -> Vec<String> {
    let mut titles: Vec<String> = canonical_tasks(core)
        .await
        .into_iter()
        .filter(|t| !t.deleted)
        .map(|t| t.title)
        .collect();
    titles.sort();
    titles
}

async fn context_names(core: &Core) -> Vec<String> {
    match core.query(Query::Contexts).await.expect("contexts") {
        QueryResult::Contexts(rows) => {
            let mut names: Vec<String> = rows.into_iter().map(|c| c.name).collect();
            names.sort();
            names
        }
        other => panic!("expected Contexts, got {other:?}"),
    }
}

/// Wait until `core`'s device list marks `device` revoked.
///
/// The revocation is an op like any other: it reaches a peer over the relay, so
/// a peer's view of it is eventually consistent and has to be waited on.
async fn wait_revoked(core: &Core, device: [u8; 16], timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = match core.query(Query::DeviceList).await.expect("device list") {
            QueryResult::Devices(rows) => rows
                .iter()
                .find(|d| d.device_id == device)
                .is_some_and(|d| d.revoked),
            other => panic!("expected Devices, got {other:?}"),
        };
        if seen {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the revocation to reach a peer"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_context(core: &Core, name: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if context_names(core).await.iter().any(|n| n == name) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the context {name:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_revocation_converges_and_the_survivors_keep_syncing() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("tmp a");
    let dir_b = tempfile::tempdir().expect("tmp b");
    let dir_c = tempfile::tempdir().expect("tmp c");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // One account, three devices: B and C join by adopting A's pairing payload,
    // which is the only way to join an account at all since ADR-0024.
    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    let c = open_paired_core(dir_c.path(), &a, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    wait_live(&c, TIMEOUT).await;

    // Baseline: all three converge before the cut.
    create_task(&a, "before the cut").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;
    wait_tasks_converge(&a, &c, 1, TIMEOUT).await;

    // The cut, which is the HLC of the op this emits.
    let c_device = c.device_id();
    a.submit(Command::RevokeDevice {
        device_id: EntityRef::new(EntityKind::Device, c_device),
        reason: RevokeReason::Lost,
    })
    .await
    .expect("revoke C");
    wait_revoked(&b, c_device, TIMEOUT).await;

    // ---- The account keeps working ----
    //
    // A writes into a Stream created after the cut — a fresh epoch, whose key
    // exists only in the envelopes the rotation emitted — and a Context, which
    // lives in the vault-meta stream, the one the rotation set exists to
    // include. B reads both, so every rotated key reached the device it was
    // meant to reach.
    let stream = create_stream(&a, "after the cut").await;
    a.submit(Command::CreateTask(TaskDraft {
        title: "after the cut".into(),
        stream_id: Some(stream),
        ..Default::default()
    }))
    .await
    .expect("create task in the new stream");
    a.submit(Command::CreateContext(ContextDraft {
        name: "post-revocation".into(),
        description: None,
    }))
    .await
    .expect("create context");

    wait_tasks_converge(&a, &b, 2, TIMEOUT).await;
    wait_context(&b, "post-revocation", TIMEOUT).await;

    // ---- What the revoked device keeps ----
    //
    // Everything. Its pre-revocation content, plainly; and — since this slice
    // enforces nothing — its ability to read and write what comes after too.
    // The assertion here is the modest one that is true: revoking a device does
    // not take away what it already had.
    assert!(
        task_titles(&c).await.contains(&"before the cut".to_owned()),
        "the pre-revocation task is still readable on the revoked device"
    );
    assert_eq!(
        task_titles(&a).await,
        task_titles(&b).await,
        "and the two surviving devices still agree with each other"
    );
}
