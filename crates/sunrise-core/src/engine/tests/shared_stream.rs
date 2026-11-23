//! What the owner's devices log inside a shared Stream for its recipients
//! (ADR-0076): writer certs per `(Stream, epoch, device)` and a copy of each
//! re-sealed grant under the epoch it replaces.

use super::share_grant::{
    deliver_where, founder, founders, rotate, shares, work_stream, Recipient,
};
use super::*;
use crate::control_op::{StreamShareGrantPayload, StreamWriterCertPayload};
use sunrise_crypto::{
    decode_envelope, decode_share_grant, open_envelope_unverified, DeviceCert, IdentityChain,
};

/// One copy in a shared Stream's log: the envelope's signer, `seq` and
/// epoch, and the op.
struct Copy {
    device: [u8; 16],
    seq: u64,
    epoch: u32,
    op: InnerOp,
}

/// Every writer cert and grant copy in `stream`'s log on `db`, by `seq`
/// per device.
fn copies(e: &Engine, db: &Db, stream: EntityRef) -> Vec<Copy> {
    let envs: Vec<Vec<u8>> = {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT envelope FROM ops WHERE stream_id = ?
                 AND inner_kind IN ('stream.writer_cert', 'stream.share_grant')
                 ORDER BY device_id, seq",
            )
            .unwrap();
        stmt.query_map(params![&stream.bytes()[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    envs.iter()
        .map(|bytes| {
            let env = decode_envelope(bytes).unwrap();
            let inner = e
                .keychain
                .stream_keys_at(&env.stream_id, env.epoch)
                .iter()
                .find_map(|k| open_envelope_unverified(&env, k).ok())
                .expect("a key this replica holds opens the copy");
            let op = decode_inner_op(&inner).unwrap();
            Copy {
                device: env.device_id,
                seq: env.seq,
                epoch: env.epoch,
                op,
            }
        })
        .collect()
}

/// The device each writer cert in `copies` names, with the epoch it is
/// logged under.
fn certs(copies: &[Copy]) -> Vec<([u8; 16], u32)> {
    copies
        .iter()
        .filter_map(|c| match &c.op {
            InnerOp::StreamWriterCert(p) => Some((
                DeviceCert::from_cbor(&p.cert).unwrap().body.device_id,
                c.epoch,
            )),
            _ => None,
        })
        .collect()
}

fn task_in(e: &Engine, db: &mut Db, stream: EntityRef, title: &str) -> u64 {
    let res = e
        .apply(
            db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                stream_id: Some(stream),
                ..Default::default()
            }),
        )
        .unwrap();
    db.conn()
        .query_row(
            "SELECT seq FROM ops WHERE op_id = ?",
            params![&res.op_id[..]],
            |r| r.get::<_, i64>(0),
        )
        .map(|s| u64::try_from(s).unwrap())
        .unwrap()
}

fn live(e: &Engine, db: &mut Db, stream: EntityRef) -> u32 {
    db.with_tx(|tx| e.ensure_stream_epoch(tx, stream.bytes(), T0))
        .unwrap()
        .0
}

/// Sharing logs, under the live epoch, the sharer's own cert and the cert of
/// each sibling that wrote into the Stream, and each carries a chain that
/// verifies from the account's genesis and reaches the cert's identity.
#[test]
fn sharing_logs_every_writers_cert_under_the_live_epoch() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    task_in(&ea, &mut dba, stream, "A writes");
    deliver_where(&ea, &dba, &eb, &mut dbb, |_| true);
    task_in(&eb, &mut dbb, stream, "B writes");
    deliver_where(&eb, &dbb, &ea, &mut dba, |_| true);

    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();

    let epoch = live(&ea, &mut dba, stream);
    let mut logged = certs(&copies(&ea, &dba, stream));
    logged.sort_unstable();
    let mut expected = vec![
        (ea.keychain.device_id(), epoch),
        (eb.keychain.device_id(), epoch),
    ];
    expected.sort_unstable();
    assert_eq!(
        logged, expected,
        "one cert per writer, under the live epoch"
    );

    for copy in copies(&ea, &dba, stream) {
        let InnerOp::StreamWriterCert(p) = copy.op else {
            panic!("only certs are logged by a share");
        };
        assert_eq!(copy.device, ea.keychain.device_id(), "the sharer logs both");
        let chain = IdentityChain::decode(&p.chain).unwrap();
        chain.verify(&chain.genesis_identity_id()).unwrap();
        let (id, pk) = chain.head();
        DeviceCert::from_cbor(&p.cert)
            .unwrap()
            .verify_binding(&pk, &id)
            .expect("the cert verifies under the chain's identity");
    }
}

/// A write into a shared Stream logs the writer's cert first, once per
/// epoch: none again in the same epoch, and one more after a rotation, at a
/// `seq` below the write's.
#[test]
fn a_write_logs_its_writers_cert_once_per_epoch_before_the_op() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    task_in(&e, &mut db, stream, "first");
    let ada = Recipient::new(0x31);
    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    let first = live(&e, &mut db, stream);
    task_in(&e, &mut db, stream, "second");
    task_in(&e, &mut db, stream, "third");
    let me = e.keychain.device_id();
    assert_eq!(certs(&copies(&e, &db, stream)), vec![(me, first)]);

    rotate(&e, &mut db, stream);
    let second = live(&e, &mut db, stream);
    assert!(second > first);
    let task_seq = task_in(&e, &mut db, stream, "after the rotation");

    let all = copies(&e, &db, stream);
    assert_eq!(certs(&all), vec![(me, first), (me, second)]);
    let cert_seq = all
        .iter()
        .find(|c| matches!(c.op, InnerOp::StreamWriterCert(_)) && c.epoch == second)
        .unwrap()
        .seq;
    assert!(cert_seq < task_seq, "the cert precedes the op it verifies");
}

/// A Stream nobody shares gets no copies, rotated or written to.
#[test]
fn an_unshared_stream_gets_no_copies() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    task_in(&e, &mut db, stream, "first");
    rotate(&e, &mut db, stream);
    task_in(&e, &mut db, stream, "second");
    assert!(copies(&e, &db, stream).is_empty());
}

/// Revoking Bob rotates: the re-seal to Ada is copied into the Stream under
/// the epoch Ada holds, beside the sharer's cert under it; Bob gets none, and
/// the copy is the very grant the vault-meta op carries.
#[test]
fn a_rotation_copies_each_reseal_into_the_stream_under_the_epoch_it_replaces() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    task_in(&e, &mut db, stream, "first");
    let (ada, bob) = (Recipient::new(0x31), Recipient::new(0x33));
    for r in [&ada, &bob] {
        e.share_stream(&mut db, stream, &r.person(), r.dh.public_bytes(), None)
            .unwrap();
    }
    let held = live(&e, &mut db, stream);
    let next = e.revoke_share(&mut db, stream, bob.identity_id()).unwrap();

    let all = copies(&e, &db, stream);
    let grants: Vec<(u32, &StreamShareGrantPayload)> = all
        .iter()
        .filter_map(|c| match &c.op {
            InnerOp::StreamShareGrant(p) => Some((c.epoch, p)),
            _ => None,
        })
        .collect();
    assert_eq!(grants.len(), 1, "one copy, for Ada alone");
    let (sealed_under, copy) = grants[0];
    assert_eq!(sealed_under, held, "sealed under the epoch Ada holds");
    let terms = decode_share_grant(&copy.grant).unwrap().terms().clone();
    assert_eq!(terms.recipient_identity_id, ada.identity_id());
    assert_eq!(terms.epoch, next);
    let ada_share = shares(&e, &db, stream)
        .into_iter()
        .find(|s| s.recipient_identity_id == ada.identity_id())
        .unwrap();
    assert_eq!(
        copy.grant, ada_share.grant,
        "the copy is the re-seal itself"
    );
    assert!(certs(&all).contains(&(e.keychain.device_id(), held)));
    let chain = IdentityChain::decode(&copy.chain).unwrap();
    chain.verify(&chain.genesis_identity_id()).unwrap();
}

/// B wrote into the Stream, and A shared it without having B's op: B logs
/// its own cert under the live epoch when it applies the share, and A's
/// copies apply on B without error.
#[test]
fn a_sibling_that_learns_of_a_share_logs_its_cert_for_what_it_wrote() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    task_in(&ea, &mut dba, stream, "A writes");
    deliver_where(&ea, &dba, &eb, &mut dbb, |_| true);
    task_in(&eb, &mut dbb, stream, "B writes");

    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    assert_eq!(
        certs(&copies(&ea, &dba, stream)),
        vec![(ea.keychain.device_id(), live(&ea, &mut dba, stream))],
        "A knows of no op of B's"
    );

    deliver_where(&ea, &dba, &eb, &mut dbb, |_| true);
    let epoch = live(&eb, &mut dbb, stream);
    let on_b = copies(&eb, &dbb, stream);
    assert!(
        on_b.iter().any(|c| c.device == eb.keychain.device_id()
            && matches!(&c.op, InnerOp::StreamWriterCert(p)
                if DeviceCert::from_cbor(&p.cert).unwrap().body.device_id
                    == eb.keychain.device_id())
            && c.epoch == epoch),
        "B logs its own cert on applying the share"
    );
}

#[test]
fn the_shared_stream_copies_round_trip_with_unknown_fields_and_are_control_ops() {
    let mut unknown = Unknowns::new();
    unknown.insert(
        "later".into(),
        sunrise_cbor::CborValue(ciborium::value::Value::Text("kept".into())),
    );
    let ops = [
        InnerOp::StreamWriterCert(StreamWriterCertPayload {
            cert: vec![1, 2, 3],
            chain: vec![4, 5],
            unknown: unknown.clone(),
        }),
        InnerOp::StreamShareGrant(StreamShareGrantPayload {
            grant: vec![6, 7],
            chain: vec![8],
            unknown,
        }),
    ];
    for (op, kind) in ops.iter().zip(["stream.writer_cert", "stream.share_grant"]) {
        let bytes = encode_inner_op(op).unwrap();
        let back = decode_inner_op(&bytes).unwrap();
        assert_eq!(encode_inner_op(&back).unwrap(), bytes);
        assert_eq!(op.inner_kind(), kind);
        assert!(op.is_control());
    }
}
