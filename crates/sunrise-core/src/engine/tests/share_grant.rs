//! Share grants as ops: emitted to a Person, folded from the op log, and
//! re-sealed by a rotation (#1318).

use super::*;
use crate::control_op::ShareGrantOpPayload;
use crate::engine::share_grant::StreamShare;
use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
use sunrise_crypto::{
    decode_share_grant, identity_id_from_pub, seal_share_grant, ShareGrantTerms, ShareRole,
    StreamMetadata,
};
use sunrise_domain::Person;

/// The identity a Stream is shared with: a stranger to the account.
struct Recipient {
    signing: IdentitySigningKeyPair,
    dh: IdentityDhKeyPair,
}

impl Recipient {
    fn new(seed: u8) -> Self {
        Self {
            signing: IdentitySigningKeyPair::from_secret_bytes(&[seed; 32]),
            dh: IdentityDhKeyPair::from_secret_bytes([seed ^ 0xff; 32]),
        }
    }

    fn identity_id(&self) -> [u8; 16] {
        identity_id_from_pub(&self.signing.public_bytes())
    }

    fn person(&self) -> Person {
        Person {
            id: EntityRef::new(EntityKind::Person, [0x9e; 16]),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            display_name: "Ada".into(),
            identity_id: Some(EntityRef::new(EntityKind::Identity, self.identity_id())),
            deleted: false,
            unknown: Unknowns::new(),
        }
    }
}

/// A device holding `ID_S_priv`, with real random Stream keys, so a rotation
/// mints a key the previous epoch's holder does not have.
fn founder(seed: [u8; 32]) -> (Engine, Db) {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    (engine_random_keys(ROOT, seed, clock), db_root(ROOT))
}

/// Two such devices of one account, each trusting the other, B holding A's
/// vault-meta key as pairing would have handed it over.
fn founders() -> (Engine, Db, Engine, Db) {
    let (ea, mut dba) = founder([1u8; 32]);
    let (eb, mut dbb) = founder([2u8; 32]);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    paired_meta(&eb, &mut dbb, &ea, &mut dba);
    (ea, dba, eb, dbb)
}

/// Mint `sponsor`'s vault-meta epoch and hand it to `device`.
fn paired_meta(device: &Engine, db: &mut Db, sponsor: &Engine, sdb: &mut Db) {
    sdb.with_tx(|tx| {
        sponsor
            .ensure_stream_epoch(tx, &META_STREAM, T0)
            .map(|_| ())
    })
    .unwrap();
    hand_over_key(device, db, sponsor, sdb, &META_STREAM);
}

fn work_stream(e: &Engine, db: &mut Db) -> EntityRef {
    e.apply(
        db,
        Command::CreateStream(StreamDraft {
            name: "Work".into(),
            color: Some(StreamColor::Sky),
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn rotate(e: &Engine, db: &mut Db, stream: EntityRef) {
    e.apply(db, Command::RotateStreamKey { stream }).unwrap();
}

/// The Stream's live epoch, minted first where it has none.
fn live_epoch(e: &Engine, db: &mut Db, stream: EntityRef) -> (u32, StreamKey) {
    db.with_tx(|tx| e.ensure_stream_epoch(tx, stream.bytes(), T0))
        .unwrap()
}

fn shares(e: &Engine, db: &Db, stream: EntityRef) -> Vec<StreamShare> {
    e.stream_shares(db, stream).unwrap()
}

/// The grant `share` carries, opened as `recipient` at `now_ms`.
fn open(e: &Engine, share: &StreamShare, recipient: &Recipient, now_ms: u64) -> StreamKey {
    decode_share_grant(&share.grant)
        .unwrap()
        .open(
            &e.keychain.identity_signing_pub(),
            &recipient.identity_id(),
            &recipient.dh,
            now_ms,
        )
        .unwrap()
        .key
}

/// Log `payload` as a `ShareGrant` op from `e`, as a sibling's would arrive.
fn log_share_op(e: &Engine, db: &mut Db, payload: ShareGrantOpPayload) {
    db.with_tx(|tx| e.emit_control_op(tx, &InnerOp::ShareGrant(payload), T0, None))
        .unwrap();
}

#[test]
fn a_share_is_logged_and_opens_for_its_recipient_only() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);

    let encoded = e
        .share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();

    let listed = shares(&e, &db, stream);
    assert_eq!(listed.len(), 1);
    let share = &listed[0];
    assert_eq!(
        share.grant, encoded,
        "the fold returns the grant the share sent"
    );
    assert_eq!(share.person, ada.person().id);
    assert_eq!(share.recipient_identity_id, ada.identity_id());
    assert_eq!(share.expires_at_ms, None);

    let (epoch, key) = live_epoch(&e, &mut db, stream);
    assert_eq!(share.epoch, epoch);
    let grant = decode_share_grant(&encoded).unwrap();
    let opened = grant
        .open(
            &e.keychain.identity_signing_pub(),
            &ada.identity_id(),
            &ada.dh,
            T0,
        )
        .unwrap();
    assert_eq!(opened.key, key, "the recipient holds the live epoch's key");
    assert_eq!(opened.metadata.name, "Work");
    assert_eq!(opened.metadata.color, "sky");

    let mallory = Recipient::new(0x32);
    assert!(
        grant
            .open(
                &e.keychain.identity_signing_pub(),
                &mallory.identity_id(),
                &mallory.dh,
                T0,
            )
            .is_err(),
        "nobody else opens it"
    );

    let kinds: Vec<String> = db
        .conn()
        .prepare("SELECT inner_kind FROM ops WHERE stream_id = ? AND inner_kind = 'share.grant'")
        .unwrap()
        .query_map(params![&META_STREAM[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kinds.len(), 1, "one control op on the vault-meta stream");
}

#[test]
fn a_rotation_reseals_the_new_epoch_to_the_recipient() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    let before = shares(&e, &db, stream)[0].epoch;

    rotate(&e, &mut db, stream);

    let (epoch, key) = live_epoch(&e, &mut db, stream);
    assert!(epoch > before, "the rotation minted a new epoch");
    let listed = shares(&e, &db, stream);
    assert_eq!(
        listed.len(),
        1,
        "a re-seal replaces the share, not adds one"
    );
    assert_eq!(listed[0].epoch, epoch, "the share moved to the new epoch");
    assert_eq!(listed[0].person, ada.person().id);
    assert_eq!(open(&e, &listed[0], &ada, T0), key);
}

#[test]
fn a_device_revocation_reseals_every_shared_stream() {
    let (ea, mut dba, eb, _) = founders();
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    let before = shares(&ea, &dba, stream)[0].epoch;
    // The Streams rotate before the identity does, so the re-seal is signed
    // by the identity the recipient already knows.
    let granter = ea.keychain.identity_signing_pub();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    let (epoch, key) = live_epoch(&ea, &mut dba, stream);
    assert!(epoch > before);
    let listed = shares(&ea, &dba, stream);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].epoch, epoch,
        "the share survives the identity rotation"
    );
    let opened = decode_share_grant(&listed[0].grant)
        .unwrap()
        .open(&granter, &ada.identity_id(), &ada.dh, T0)
        .unwrap();
    assert_eq!(opened.key, key);
}

#[test]
fn an_expired_share_is_not_resealed() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_random_keys(ROOT, [1u8; 32], clock.clone());
    let mut db = db_root(ROOT);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    let grace = Recipient::new(0x33);
    let mut grace_person = grace.person();
    grace_person.id = EntityRef::new(EntityKind::Person, [0x9f; 16]);
    e.share_stream(
        &mut db,
        stream,
        &ada.person(),
        ada.dh.public_bytes(),
        Some(T0 + 1_000),
    )
    .unwrap();
    e.share_stream(
        &mut db,
        stream,
        &grace_person,
        grace.dh.public_bytes(),
        None,
    )
    .unwrap();
    let before = live_epoch(&e, &mut db, stream).0;

    set_clock(&clock, T0 + 1_000);
    rotate(&e, &mut db, stream);

    let (epoch, _) = live_epoch(&e, &mut db, stream);
    let by_recipient: Vec<(u32, Option<u64>)> = shares(&e, &db, stream)
        .iter()
        .map(|s| (s.epoch, s.expires_at_ms))
        .collect();
    let mut expected = vec![(before, Some(T0 + 1_000)), (epoch, None)];
    if ada.identity_id() > grace.identity_id() {
        expected.reverse();
    }
    assert_eq!(
        by_recipient, expected,
        "the expired share stays at the epoch it had, and the other moves on"
    );
}

#[test]
fn a_reseal_carries_the_expiry_unchanged() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    let until = T0 + 86_400_000;
    e.share_stream(
        &mut db,
        stream,
        &ada.person(),
        ada.dh.public_bytes(),
        Some(until),
    )
    .unwrap();
    rotate(&e, &mut db, stream);
    let share = &shares(&e, &db, stream)[0];
    assert_eq!(share.expires_at_ms, Some(until));
    assert_eq!(
        decode_share_grant(&share.grant)
            .unwrap()
            .terms()
            .expires_at_ms,
        Some(until)
    );
}

#[test]
fn a_sibling_folds_the_share_and_its_rotation_reseals_it() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&ea, &dba, &eb, &mut dbb, true);

    let shared = shares(&ea, &dba, stream);
    assert_eq!(shared.len(), 1);
    assert_eq!(
        shares(&eb, &dbb, stream),
        shared,
        "both replicas fold the same share from the same ops"
    );

    rotate(&eb, &mut dbb, stream);
    deliver(&eb, &dbb, &ea, &mut dba, true);
    let (epoch, key) = live_epoch(&eb, &mut dbb, stream);
    assert!(epoch > shared[0].epoch);
    let on_a = shares(&ea, &dba, stream);
    assert_eq!(on_a, shares(&eb, &dbb, stream));
    assert_eq!(on_a[0].epoch, epoch, "the sibling's re-seal reached A");
    assert_eq!(open(&ea, &on_a[0], &ada, T0), key);
}

#[test]
fn a_paired_device_cannot_share_and_its_rotation_reseals_nothing() {
    let (founder, mut fdb) = founder([1u8; 32]);
    let paired = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        seeded_rng(),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [2u8; 32],
        )),
    );
    let mut db = db_root(ROOT);
    trust(&paired, &mut db, &founder);
    trust(&founder, &mut fdb, &paired);
    paired_meta(&paired, &mut db, &founder, &mut fdb);
    let stream = work_stream(&paired, &mut db);
    let ada = Recipient::new(0x31);

    let before = op_count(&db);
    invalid(
        paired.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None),
        "did not seal",
    );
    assert_eq!(op_count(&db), before, "a refused share writes nothing");

    // A share the founder made reaches the paired device's fold, and the
    // paired device's own rotation cannot sign its re-seal.
    deliver(&paired, &db, &founder, &mut fdb, true);
    founder
        .share_stream(&mut fdb, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&founder, &fdb, &paired, &mut db, true);
    let shared_at = shares(&paired, &db, stream)[0].epoch;
    rotate(&paired, &mut db, stream);
    assert!(live_epoch(&paired, &mut db, stream).0 > shared_at);
    assert_eq!(
        shares(&paired, &db, stream)[0].epoch,
        shared_at,
        "no re-seal without ID_S_priv"
    );
}

#[test]
fn a_share_is_refused_before_anything_is_written() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    let pk = ada.dh.public_bytes();
    let before = op_count(&db);

    invalid(
        e.share_stream(&mut db, ada.person().id, &ada.person(), pk, None),
        "not a Stream",
    );
    invalid(
        e.share_stream(
            &mut db,
            EntityRef::new(EntityKind::Stream, META_STREAM),
            &ada.person(),
            pk,
            None,
        ),
        "not a Stream",
    );
    let mut nameless = ada.person();
    nameless.identity_id = None;
    invalid(
        e.share_stream(&mut db, stream, &nameless, pk, None),
        "no identity",
    );
    let mut gone = ada.person();
    gone.deleted = true;
    invalid(
        e.share_stream(&mut db, stream, &gone, pk, None),
        "not deleted",
    );
    let mut me = ada.person();
    me.identity_id = Some(EntityRef::new(
        EntityKind::Identity,
        e.keychain.identity_id(),
    ));
    invalid(
        e.share_stream(&mut db, stream, &me, pk, None),
        "own identity",
    );
    invalid(
        e.share_stream(&mut db, stream, &ada.person(), pk, Some(T0)),
        "expired",
    );
    assert!(matches!(
        e.share_stream(&mut db, stream_ref(0x77), &ada.person(), pk, None),
        Err(EngineError::NotFound(_))
    ));
    assert_eq!(op_count(&db), before, "no refusal writes an op");
}

#[test]
fn the_fold_believes_only_a_grant_the_account_signed_to_a_person() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let (epoch, key) = live_epoch(&e, &mut db, stream);
    let ada = Recipient::new(0x31);
    let terms = ShareGrantTerms {
        stream_id: *stream.bytes(),
        epoch,
        recipient_identity_id: ada.identity_id(),
        role: ShareRole::Viewer,
        expires_at_ms: None,
    };
    let metadata = StreamMetadata {
        name: "Work".into(),
        color: "sky".into(),
    };
    let seal = |signer: &IdentitySigningKeyPair| {
        seal_share_grant(
            signer,
            terms.clone(),
            &ada.dh.public_bytes(),
            &key,
            &metadata,
            &mut rand::rngs::OsRng,
        )
        .unwrap()
        .encode()
        .unwrap()
    };
    let payload = |person: EntityRef, grant: Vec<u8>| ShareGrantOpPayload {
        person,
        recipient_id_d_pub: ada.dh.public_bytes(),
        grant,
        unknown: Unknowns::new(),
    };

    // Signed by an identity that is not the account's.
    let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x44; 32]);
    log_share_op(&e, &mut db, payload(ada.person().id, seal(&stranger)));
    // Not a grant at all.
    log_share_op(&e, &mut db, payload(ada.person().id, vec![0xa0]));
    assert!(shares(&e, &db, stream).is_empty());

    // The account's own grant, filed under something that is not a Person.
    // `[0x5a; 32]` is every test keychain's `ID_S_priv`.
    let account_key = IdentitySigningKeyPair::from_secret_bytes(&[0x5a; 32]);
    assert_eq!(
        account_key.public_bytes(),
        e.keychain.identity_signing_pub()
    );
    let account = seal(&account_key);
    log_share_op(&e, &mut db, payload(stream, account.clone()));
    assert!(shares(&e, &db, stream).is_empty());

    // And filed under the Person.
    log_share_op(&e, &mut db, payload(ada.person().id, account));
    assert_eq!(shares(&e, &db, stream).len(), 1);
    assert!(
        shares(&e, &db, stream_ref(0x77)).is_empty(),
        "a share is the Stream's its grant names"
    );
}

#[test]
fn the_share_grant_op_round_trips_with_unknown_fields() {
    let mut unknown = Unknowns::new();
    unknown.insert(
        "later".into(),
        sunrise_cbor::CborValue(ciborium::value::Value::Text("kept".into())),
    );
    let op = InnerOp::ShareGrant(ShareGrantOpPayload {
        person: EntityRef::new(EntityKind::Person, [0x9e; 16]),
        recipient_id_d_pub: [0x11; 32],
        grant: vec![1, 2, 3],
        unknown,
    });
    let bytes = encode_inner_op(&op).unwrap();
    let InnerOp::ShareGrant(back) = decode_inner_op(&bytes).unwrap() else {
        panic!("decoded as another family");
    };
    assert_eq!(encode_inner_op(&InnerOp::ShareGrant(back)).unwrap(), bytes);
    assert_eq!(op.inner_kind(), "share.grant");
    assert!(op.is_control());
}
