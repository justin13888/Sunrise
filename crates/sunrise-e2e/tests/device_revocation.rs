//! Revoking a device, end to end, over a live relay.
//!
//! Three devices on one account: A invites B and C, then revokes C. Three
//! things must follow, and this test asserts all three:
//!
//! - **The account keeps working.** Revocation mints a fresh epoch for every
//!   stream in the rotation set — the vault-meta stream and the Inbox included,
//!   not only user Streams — and seals each one to every *surviving* device and
//!   to the account identity. If any of that were wrong the surviving devices
//!   would go dark, so B converging on a Stream and a Context created after the
//!   cut is the assertion that rotation and redistribution actually work.
//! - **The revoked device cannot read past the cut.** C is sealed no envelope
//!   for the new epochs, and there is no identity copy for it to open instead,
//!   so the Stream and Context A creates afterwards never become readable on C.
//! - **It keeps what it already had.** No rotation can take that back: C holds
//!   every key it held, so the pre-revocation task stays readable on C
//!   throughout. Revocation is forward-only and this is what that means.
//!
//! # How the read bound is actually achieved
//!
//! Two mechanisms, and either one alone is vacuous — which is why an earlier
//! slice shipped with neither and filed #76 rather than half of it.
//!
//! `key_envelope` ops are sealed to two recipient classes (ADR-0024 decision
//! 4): each device's `D_D_pub`, and the account identity's `ID_D_pub`. The
//! identity copy is what lets a recovery with no surviving device restore
//! readable content rather than an empty vault (`docs/03-crypto/recovery.md`
//! §8), so it cannot simply be dropped. While pairing also handed every device
//! `ID_D_priv`, excluding C from the device recipients withheld nothing: C
//! opened the identity copy and read straight through the rotation.
//!
//! So both moved together. `PairingPayload` no longer carries `ID_D_priv` — it
//! exists only inside the recovery blob, behind the BIP-39 code — and
//! `emit_key_envelopes` anti-joins the revocation register. Sealing needs only
//! the public half, so the identity copy is still emitted and recovery still
//! reaches every epoch; C simply cannot open it.
//!
//! # What this test deliberately does not assert, and why
//!
//! It does not assert that C's *writes* are refused by B or by A. They are not,
//! and that is a decision rather than a gap: refusing at apply time is not
//! convergent, because a replica that applied an op before the revocation
//! arrived cannot un-apply it and this engine has no projection rebuild. What
//! bounds C's writes is the relay, which stops accepting its uploads once the
//! revocation reaches it — so a peer never sees the op to refuse. A convergent
//! peer-side check is #82.
//!
//! It also does not assert forward secrecy against the *account creator*. That
//! device, and one restored from the recovery code, hold `ID_D_priv` and can
//! open the identity copy of any epoch. `Command::RevokeDevice` refuses to
//! revoke the device it runs on, so this is only reachable by revoking the
//! creator from another device; until the recovery blob is built there is
//! nowhere else for that key to live. `docs/03-crypto/key-rotation.md`
//! §Revocation states it.
//!
//! Separately: a revoked device still holds `ID_S_priv`, so it can issue itself
//! a fresh, valid `DeviceCert` under a new device id. Revocation names a
//! device, and the identity keys are what name devices. What stands against it
//! today is the relay, which will not accept a revoked device's upload of that
//! cert.

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

    // ---- What the revoked device keeps, and what it does not ----
    //
    // Keeps: everything it already had. No rotation can take that back, and
    // pretending otherwise would be the dishonest assertion here.
    assert!(
        task_titles(&c).await.contains(&"before the cut".to_owned()),
        "the pre-revocation task is still readable on the revoked device"
    );

    // Does not keep: anything written after the cut. B has both tasks by now
    // (`wait_tasks_converge` above), so the ops have been fanned out and C has
    // had every chance the relay gives it. C is sealed no envelope for the new
    // epochs and has no `ID_D_priv` to open the identity copy with, so the
    // second task cannot materialize there.
    //
    // Given a generous settle: this is the assertion most worth being sure
    // about, and a false pass here would be a claim that revocation works when
    // it does not.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let c_titles = task_titles(&c).await;
    assert!(
        !c_titles.contains(&"after the cut".to_owned()),
        "a revoked device must not read what was written after its cut; saw {c_titles:?}"
    );
    assert!(
        !context_names(&c)
            .await
            .contains(&"post-revocation".to_owned()),
        "nor anything in the vault-meta stream, which rotates with the rest"
    );

    assert_eq!(
        task_titles(&a).await,
        task_titles(&b).await,
        "and the two surviving devices still agree with each other"
    );
}
