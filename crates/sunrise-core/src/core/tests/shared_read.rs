//! A recipient reads the owner's shared Stream from its envelopes alone: the
//! writer certs and grant copies the owner logs there (ADR-0076).

use super::*;
use crate::core::{SharedPending, SharedRead, SharedStreamReader};
use sunrise_domain::{StreamDraft, TaskDraft};

const T: u64 = 1_700_000_000_000;

async fn account(dir: &std::path::Path) -> Core {
    Core::open(cfg(dir), unlock()).await.unwrap()
}

/// A recipient whose clock the test moves.
async fn account_at(dir: &std::path::Path, clock: &Arc<FakeClock>) -> Core {
    crate::engine::check_rebuild_after_every_write();
    let config = CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::clone(clock) as Arc<dyn crate::config::Clock>,
        seeded_rng(),
    );
    Core::open(config, unlock()).await.unwrap()
}

async fn stream(core: &Core) -> sunrise_id::EntityRef {
    core.submit(Command::CreateStream(StreamDraft {
        name: "Garden".into(),
        ..Default::default()
    }))
    .await
    .unwrap()
    .entity
}

async fn task(core: &Core, s: sunrise_id::EntityRef, title: &str) {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        stream_id: Some(s),
        ..Default::default()
    }))
    .await
    .unwrap();
}

/// The owner's envelopes in `s`, in its log's order.
fn envelopes(owner: &Core, s: sunrise_id::EntityRef) -> Vec<Vec<u8>> {
    let db = owner.db.lock();
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE stream_id = ? ORDER BY seq")
        .unwrap();
    stmt.query_map(rusqlite::params![&s.bytes()[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn read_all(
    recipient: &Core,
    reader: &mut SharedStreamReader,
    envs: &[Vec<u8>],
) -> Vec<SharedRead> {
    envs.iter()
        .map(|e| recipient.read_shared_envelope(reader, e).unwrap())
        .collect()
}

fn ops(reads: &[SharedRead]) -> Vec<&'static str> {
    reads
        .iter()
        .filter_map(|r| match r {
            SharedRead::Op { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

/// Shares `s` with `recipient`, which accepts it.
fn share_and_accept(
    owner: &Core,
    s: sunrise_id::EntityRef,
    recipient: &Core,
    expires_at_ms: Option<u64>,
) {
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            expires_at_ms,
        )
        .unwrap();
    recipient
        .accept_share(&owner.identity_signing_pub(), &grant, [0x5a; 16])
        .unwrap();
}

/// The recipient verifies the owner's ops under the writer cert the share
/// logged, and an op read before its writer's cert waits and then reads.
#[tokio::test]
async fn a_recipient_reads_the_owners_ops_under_the_writer_cert_in_the_stream() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner).await;
    task(&owner, s, "before the share").await;
    share_and_accept(&owner, s, &recipient, None);
    task(&owner, s, "after the share").await;

    let envs = envelopes(&owner, s);
    let mut reader = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
    let me = owner.engine.keychain().device_id();
    assert_eq!(
        recipient
            .read_shared_envelope(&mut reader, &envs[0])
            .unwrap(),
        SharedRead::Pending(SharedPending::UnknownWriter { device_id: me }),
        "the first op waits for its writer's cert"
    );
    let reads = read_all(&recipient, &mut reader, &envs);
    assert!(reads.contains(&SharedRead::WriterCert { device_id: me }));
    assert!(reader.believes_writer(&me));
    assert_eq!(
        ops(&reads),
        vec!["task.patch"],
        "the op after the cert reads"
    );
    assert_eq!(
        ops(&read_all(&recipient, &mut reader, &envs)),
        vec!["task.patch", "task.patch"],
        "read again, the waiting op reads too"
    );
}

/// A reader that names another identity as the granter believes no cert of
/// the owner's: the chain the cert carries does not hold that identity.
#[tokio::test]
async fn a_writer_cert_whose_chain_lacks_the_granter_is_refused() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let owner = account(dirs[0].path()).await;
    let recipient = account(dirs[1].path()).await;
    let other = account(dirs[2].path()).await;
    let s = stream(&owner).await;
    task(&owner, s, "one").await;
    share_and_accept(&owner, s, &recipient, None);
    let cert = envelopes(&owner, s)
        .into_iter()
        .find(|e| {
            let mut probe = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
            matches!(
                recipient.read_shared_envelope(&mut probe, e),
                Ok(SharedRead::WriterCert { .. })
            )
        })
        .expect("the share logged a writer cert");

    // The held key is filed under the identity the share was accepted from;
    // a reader naming another one finds the cert's chain does not hold it.
    let mut wrong = SharedStreamReader::new(other.identity_signing_pub(), *s.bytes());
    let held = recipient.held_shares().unwrap()[0].clone();
    {
        let db = recipient.db.lock();
        let key = recipient
            .engine
            .keychain()
            .held_share_key(&db, &owner.identity_id(), s.bytes(), held.epoch)
            .unwrap()
            .unwrap();
        recipient
            .engine
            .keychain()
            .hold_share_key(
                &db,
                &crate::keychain::HoldShareKey {
                    granter_identity_id: other.identity_id(),
                    stream_id: *s.bytes(),
                    epoch: held.epoch,
                    relay_grant_id: [2; 16],
                    name: "x",
                    color: "sky",
                    expires_at_ms: None,
                    accepted_at_ms: T,
                },
                &key,
                recipient.rng(),
            )
            .unwrap();
    }
    assert!(matches!(
        recipient.read_shared_envelope(&mut wrong, &cert),
        Err(CoreError::Engine(EngineError::RemoteOpInvalid(_)))
    ));
    assert!(!wrong.believes_writer(&owner.engine.keychain().device_id()));
}

/// The owner revokes a second recipient: the rotation's grant copy reaches
/// the first through the Stream, which then holds the new epoch and reads
/// what the owner writes under it. The revoked one reads no copy to it and
/// holds no key at the new epoch.
#[tokio::test]
async fn a_rotations_grant_copy_carries_the_recipient_to_the_next_epoch() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let owner = account(dirs[0].path()).await;
    let ada = account(dirs[1].path()).await;
    let bob = account(dirs[2].path()).await;
    let s = stream(&owner).await;
    task(&owner, s, "one").await;
    share_and_accept(&owner, s, &ada, None);
    share_and_accept(&owner, s, &bob, None);
    let next = owner.revoke_share(s, bob.identity_id()).unwrap();
    task(&owner, s, "under the next epoch").await;

    let envs = envelopes(&owner, s);
    let mut reader = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
    let first = read_all(&ada, &mut reader, &envs);
    assert!(first.contains(&SharedRead::EpochHeld { epoch: next }));
    let again = read_all(&ada, &mut reader, &envs);
    assert_eq!(
        ops(&again),
        vec!["task.patch", "task.patch"],
        "with the next epoch held, the op under it reads"
    );
    let held = ada.held_shares().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].epoch, next);
    assert_eq!(held[0].relay_grant_ids, vec![[0x5a; 16]]);

    let mut bobs = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
    let bob_reads = read_all(&bob, &mut bobs, &envs);
    assert!(!bob_reads
        .iter()
        .any(|r| matches!(r, SharedRead::EpochHeld { .. })));
    assert!(bob_reads.contains(&SharedRead::OtherRecipient));
    assert!(bob_reads.contains(&SharedRead::Pending(SharedPending::NoKey { epoch: next })));
}

/// Past the share's expiry on the recipient's clock nothing is read,
/// whatever arrives.
#[tokio::test]
async fn nothing_is_read_once_the_share_has_expired() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let clock = Arc::new(FakeClock(PLMutex::new(T)));
    let owner = account(da.path()).await;
    let recipient = account_at(db.path(), &clock).await;
    let s = stream(&owner).await;
    task(&owner, s, "one").await;
    share_and_accept(&owner, s, &recipient, Some(T + 60_000));
    let envs = envelopes(&owner, s);
    let mut reader = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
    assert!(!read_all(&recipient, &mut reader, &envs).contains(&SharedRead::Expired));

    *clock.0.lock() = T + 60_000;
    assert!(read_all(&recipient, &mut reader, &envs)
        .iter()
        .all(|r| *r == SharedRead::Expired));
}

/// A read with no share held, and an envelope of another Stream, are
/// refused.
#[tokio::test]
async fn a_read_needs_a_held_share_of_that_stream() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner).await;
    task(&owner, s, "one").await;
    let mut reader = SharedStreamReader::new(owner.identity_signing_pub(), *s.bytes());
    let envs = envelopes(&owner, s);
    assert!(matches!(
        recipient.read_shared_envelope(&mut reader, &envs[0]),
        Err(CoreError::Engine(EngineError::Invalid(_)))
    ));

    share_and_accept(&owner, s, &recipient, None);
    let other = stream(&owner).await;
    task(&owner, other, "elsewhere").await;
    assert!(matches!(
        recipient.read_shared_envelope(&mut reader, &envelopes(&owner, other)[0]),
        Err(CoreError::Engine(EngineError::RemoteOpInvalid(_)))
    ));
}
