//! Share grants as ops: emitted to a Person, folded from the op log, and
//! re-sealed by a rotation (#1318).

use super::*;
use crate::control_op::{
    ResealRecipient, ShareDeclinePayload, ShareGrantOpPayload, ShareResealRequestPayload,
    ShareRevokePayload,
};
use crate::engine::share_grant::{EndedShare, ShareEnd, StreamShare};
use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
use sunrise_crypto::{
    decode_share_grant, identity_id_from_pub, seal_share_grant, ShareGrantTerms, ShareRole,
    StreamMetadata,
};
use sunrise_domain::Person;

/// The identity a Stream is shared with: a stranger to the account.
pub(super) struct Recipient {
    pub(super) signing: IdentitySigningKeyPair,
    pub(super) dh: IdentityDhKeyPair,
}

impl Recipient {
    pub(super) fn new(seed: u8) -> Self {
        Self {
            signing: IdentitySigningKeyPair::from_secret_bytes(&[seed; 32]),
            dh: IdentityDhKeyPair::from_secret_bytes([seed ^ 0xff; 32]),
        }
    }

    pub(super) fn identity_id(&self) -> [u8; 16] {
        identity_id_from_pub(&self.signing.public_bytes())
    }

    pub(super) fn person(&self) -> Person {
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
pub(super) fn founder(seed: [u8; 32]) -> (Engine, Db) {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    (engine_random_keys(ROOT, seed, clock), db_root(ROOT))
}

/// Two such devices of one account, each trusting the other, B holding A's
/// vault-meta key as pairing would have handed it over.
pub(super) fn founders() -> (Engine, Db, Engine, Db) {
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

pub(super) fn work_stream(e: &Engine, db: &mut Db) -> EntityRef {
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

pub(super) fn rotate(e: &Engine, db: &mut Db, stream: EntityRef) {
    e.apply(db, Command::RotateStreamKey { stream }).unwrap();
}

/// The Stream's live epoch, minted first where it has none.
fn live_epoch(e: &Engine, db: &mut Db, stream: EntityRef) -> (u32, StreamKey) {
    db.with_tx(|tx| e.ensure_stream_epoch(tx, stream.bytes(), T0))
        .unwrap()
}

pub(super) fn shares(e: &Engine, db: &Db, stream: EntityRef) -> Vec<StreamShare> {
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
        reseal: false,
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
        reseal: true,
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

/// Bound `device`'s reads on the replica `db` holds, as applying its
/// revocation would.
fn bound(db: &mut Db, device: &Engine) {
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO device_read_bounds (device_id, first_bound_at_ms) VALUES (?, 1)",
            params![&device.keychain.device_id()[..]],
        )
        .map(|_| ())
    })
    .unwrap();
}

/// A `ShareGrant` op sharing `stream` at `epoch` with `to`, signed by
/// `signer`.
fn grant_op(
    signer: &IdentitySigningKeyPair,
    stream: EntityRef,
    (epoch, key): (u32, &StreamKey),
    to: &Recipient,
) -> ShareGrantOpPayload {
    let terms = ShareGrantTerms {
        stream_id: *stream.bytes(),
        epoch,
        recipient_identity_id: to.identity_id(),
        role: ShareRole::Viewer,
        expires_at_ms: None,
    };
    let metadata = StreamMetadata {
        name: "Work".into(),
        color: "sky".into(),
    };
    let grant = seal_share_grant(
        signer,
        terms,
        &to.dh.public_bytes(),
        key,
        &metadata,
        &mut rand::rngs::OsRng,
    )
    .unwrap()
    .encode()
    .unwrap();
    ShareGrantOpPayload {
        person: to.person().id,
        recipient_id_d_pub: to.dh.public_bytes(),
        grant,
        reseal: false,
        unknown: Unknowns::new(),
    }
}

/// The recipient identities `stream`'s shares name on `db`'s replica.
fn recipients(e: &Engine, db: &Db, stream: EntityRef) -> Vec<[u8; 16]> {
    let mut ids: Vec<[u8; 16]> = shares(e, db, stream)
        .iter()
        .map(|s| s.recipient_identity_id)
        .collect();
    ids.sort_unstable();
    ids
}

/// The share `stream` holds on `db`'s replica reaches Ada's key at the live
/// epoch, and Mallory's key opens none of it.
fn reaches_only_ada(
    e: &Engine,
    db: &mut Db,
    stream: EntityRef,
    granter: &[u8; 32],
    (ada, mallory): (&Recipient, &Recipient),
) {
    let (epoch, key) = live_epoch(e, db, stream);
    let listed = shares(e, db, stream);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].epoch, epoch, "the share is at the live epoch");
    assert_eq!(listed[0].recipient_id_d_pub, ada.dh.public_bytes());
    let grant = decode_share_grant(&listed[0].grant).unwrap();
    let opened = grant
        .open(granter, &ada.identity_id(), &ada.dh, T0)
        .unwrap();
    assert_eq!(opened.key, key);
    // Mallory holds the key she named, claiming Ada's identity.
    assert!(grant
        .open(granter, &ada.identity_id(), &mallory.dh, T0)
        .is_err());
}

/// A device re-files the head's grant naming a key it holds, then the
/// account device rotates while that device still reads, and only then is the
/// device bounded and revoked. The signature covers the key the grant is
/// sealed to, so the re-file counts for nothing from the start: neither the
/// intervening re-seal nor the revocation's carries the device's key forward.
#[test]
fn a_refiled_grant_naming_another_key_is_never_resealed_to_it() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&ea, &dba, &eb, &mut dbb, true);

    // B re-files the grant A filed, which the head signed, naming a key B
    // holds, stamped after A's.
    let filed = shares(&eb, &dbb, stream).remove(0);
    let mallory = Recipient::new(0x32);
    log_share_op(
        &eb,
        &mut dbb,
        ShareGrantOpPayload {
            person: filed.person,
            recipient_id_d_pub: mallory.dh.public_bytes(),
            grant: filed.grant.clone(),
            reseal: false,
            unknown: Unknowns::new(),
        },
    );
    deliver(&eb, &dbb, &ea, &mut dba, true);
    assert_eq!(
        shares(&ea, &dba, stream),
        vec![filed],
        "from an unbounded sender, a grant beside a key it was not sealed to counts for nothing"
    );

    // A rotation while B still reads.
    let parties = (&ada, &mallory);
    let granter = ea.keychain.identity_signing_pub();
    rotate(&ea, &mut dba, stream);
    reaches_only_ada(&ea, &mut dba, stream, &granter, parties);

    // B is bounded, and its revocation rotates every Stream again, before the
    // identity, so the re-seal is signed by the identity Ada already knows.
    bound(&mut dba, &eb);
    reaches_only_ada(&ea, &mut dba, stream, &granter, parties);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    reaches_only_ada(&ea, &mut dba, stream, &granter, parties);
}

#[test]
fn a_retired_identitys_grant_counts_only_from_an_unbounded_sender() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    let (epoch, key) = live_epoch(&ea, &mut dba, stream);
    let ada = Recipient::new(0x31);
    let bob = Recipient::new(0x33);
    // `[0x5a; 32]` is every test keychain's `ID_S_priv`.
    let retiring = IdentitySigningKeyPair::from_secret_bytes(&[0x5a; 32]);
    assert_eq!(retiring.public_bytes(), ea.keychain.identity_signing_pub());

    // A files Ada's grant and B files Bob's, both under the identity in force.
    log_share_op(
        &ea,
        &mut dba,
        grant_op(&retiring, stream, (epoch, &key), &ada),
    );
    log_share_op(
        &eb,
        &mut dbb,
        grant_op(&retiring, stream, (epoch, &key), &bob),
    );
    deliver(&eb, &dbb, &ea, &mut dba, true);
    ea.apply(
        &mut dba,
        Command::RotateIdentity {
            keep_recovery_code: true,
        },
    )
    .unwrap();
    assert_ne!(
        ea.keychain.identity_signing_pub(),
        retiring.public_bytes(),
        "the identity that signed both grants is retired"
    );

    let mut both = vec![ada.identity_id(), bob.identity_id()];
    both.sort_unstable();
    assert_eq!(
        recipients(&ea, &dba, stream),
        both,
        "a retired identity's grant counts from an unbounded sender"
    );
    bound(&mut dba, &eb);
    assert_eq!(
        recipients(&ea, &dba, stream),
        vec![ada.identity_id()],
        "and not from a bounded one"
    );
}

#[test]
fn retiring_the_recovery_code_reseals_each_share_under_the_successor() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    let before = shares(&e, &db, stream)[0].epoch;
    let retired = e.keychain.identity_signing_pub();

    e.apply(
        &mut db,
        Command::RotateIdentity {
            keep_recovery_code: false,
        },
    )
    .unwrap();
    let successor = e.keychain.identity_signing_pub();
    assert_ne!(successor, retired);

    let (epoch, key) = live_epoch(&e, &mut db, stream);
    assert!(epoch > before, "retiring the code re-mints the Stream");
    let listed = shares(&e, &db, stream);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].epoch, epoch,
        "the fold takes the successor's re-seal as the share"
    );
    let grant = decode_share_grant(&listed[0].grant).unwrap();
    let to = ada.dh.public_bytes();
    assert!(
        grant.verify(&successor, &to).is_ok(),
        "signed by the successor"
    );
    assert!(grant.verify(&retired, &to).is_err());
    assert_eq!(open(&e, &listed[0], &ada, T0), key);
}

// ---------------------------------------------------------------------------
// Revoking and declining a share (#1325)
// ---------------------------------------------------------------------------

fn ended(e: &Engine, db: &Db, stream: EntityRef) -> Vec<EndedShare> {
    e.ended_stream_shares(db, stream).unwrap()
}

/// Every `ShareGrant` op `db` holds that `recipient` is sealed at `epoch`.
fn grants_at(e: &Engine, db: &Db, recipient: &Recipient, epoch: u32) -> usize {
    let envs: Vec<Vec<u8>> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE inner_kind = 'share.grant'")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    envs.iter()
        .filter_map(|env| e.keychain.open_op(env).ok())
        .filter_map(|cbor| match decode_inner_op(&cbor) {
            Ok(InnerOp::ShareGrant(p)) => {
                Some(decode_share_grant(&p.grant).unwrap().terms().clone())
            }
            _ => None,
        })
        .filter(|t| t.recipient_identity_id == recipient.identity_id() && t.epoch == epoch)
        .count()
}

/// The founder shares with Ada and Bob, then revokes Ada: the revocation
/// rotates, the rotation's re-seal reaches only Bob, and so does every later
/// one.
#[test]
fn a_revoked_share_is_left_out_of_every_later_rotation() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let (ada, bob) = (Recipient::new(0x31), Recipient::new(0x33));
    for r in [&ada, &bob] {
        e.share_stream(&mut db, stream, &r.person(), r.dh.public_bytes(), None)
            .unwrap();
    }
    let shared_at = live_epoch(&e, &mut db, stream).0;

    let minted = e.revoke_share(&mut db, stream, ada.identity_id()).unwrap();
    assert!(minted > shared_at, "a revocation rotates the Stream");
    assert_eq!(live_epoch(&e, &mut db, stream).0, minted);
    assert_eq!(recipients(&e, &db, stream), vec![bob.identity_id()]);
    assert_eq!(shares(&e, &db, stream)[0].epoch, minted);
    let gone = ended(&e, &db, stream);
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].recipient_identity_id, ada.identity_id());
    assert_eq!(gone[0].end, ShareEnd::Revoked);
    assert_eq!(gone[0].at_ms, T0);

    rotate(&e, &mut db, stream);
    let later = live_epoch(&e, &mut db, stream).0;
    assert_eq!(shares(&e, &db, stream)[0].epoch, later);
    for epoch in [minted, later] {
        assert_eq!(grants_at(&e, &db, &ada, epoch), 0, "nothing reaches Ada");
        assert_eq!(grants_at(&e, &db, &bob, epoch), 1);
    }

    let before = op_count(&db);
    invalid(
        e.revoke_share(&mut db, stream, ada.identity_id()),
        "no share in force",
    );
    invalid(
        e.record_share_decline(&mut db, stream, [0x77; 16]),
        "no share in force",
    );
    invalid(
        e.revoke_share(
            &mut db,
            EntityRef::new(EntityKind::Stream, META_STREAM),
            bob.identity_id(),
        ),
        "not a Stream that can be shared",
    );
    assert_eq!(op_count(&db), before, "a refused end writes nothing");
}

/// Sharing again after a revocation is a fresh grant, which the fold takes
/// over the revocation.
#[test]
fn a_share_revoked_then_shared_again_is_in_force() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    e.revoke_share(&mut db, stream, ada.identity_id()).unwrap();
    assert!(shares(&e, &db, stream).is_empty());

    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    assert_eq!(recipients(&e, &db, stream), vec![ada.identity_id()]);
    assert!(ended(&e, &db, stream).is_empty());
    rotate(&e, &mut db, stream);
    let (epoch, key) = live_epoch(&e, &mut db, stream);
    assert_eq!(shares(&e, &db, stream)[0].epoch, epoch);
    assert_eq!(open(&e, &shares(&e, &db, stream)[0], &ada, T0), key);
}

/// A recorded decline ends the share with no rotation of its own, and the
/// next rotation leaves the recipient out.
#[test]
fn a_declined_share_is_ended_and_not_resealed() {
    let (e, mut db) = founder([1u8; 32]);
    let stream = work_stream(&e, &mut db);
    let ada = Recipient::new(0x31);
    e.share_stream(&mut db, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    let shared_at = live_epoch(&e, &mut db, stream).0;

    e.record_share_decline(&mut db, stream, ada.identity_id())
        .unwrap();
    assert_eq!(live_epoch(&e, &mut db, stream).0, shared_at, "no rotation");
    assert!(shares(&e, &db, stream).is_empty());
    let gone = ended(&e, &db, stream);
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].end, ShareEnd::Declined);

    rotate(&e, &mut db, stream);
    let later = live_epoch(&e, &mut db, stream).0;
    assert_eq!(grants_at(&e, &db, &ada, later), 0);
}

/// Two account devices whose clocks read `a_ms` and `b_ms`.
fn founders_at(a_ms: u64, b_ms: u64) -> (Engine, Db, Engine, Db) {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(a_ms))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(b_ms))));
    let (mut dba, mut dbb) = (db_root(ROOT), db_root(ROOT));
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    paired_meta(&eb, &mut dbb, &ea, &mut dba);
    (ea, dba, eb, dbb)
}

/// B rotates before it has seen A's revocation, so its re-seal to Ada is
/// stamped after the revocation. A re-seal never revives an ended share:
/// once both replicas hold both, neither has Ada's share in force, and the
/// next rotation on either leaves her out.
#[test]
fn a_reseal_stamped_after_a_revocation_does_not_revive_the_share() {
    let (ea, mut dba, eb, mut dbb) = founders_at(T0, T0 + 60_000);
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&ea, &dba, &eb, &mut dbb, true);

    ea.revoke_share(&mut dba, stream, ada.identity_id())
        .unwrap();
    rotate(&eb, &mut dbb, stream);
    let raced = live_epoch(&eb, &mut dbb, stream).0;
    assert_eq!(grants_at(&eb, &dbb, &ada, raced), 1, "B re-sealed to Ada");

    deliver(&ea, &dba, &eb, &mut dbb, true);
    deliver(&eb, &dbb, &ea, &mut dba, true);
    for (e, db) in [(&ea, &mut dba), (&eb, &mut dbb)] {
        assert!(shares(e, db, stream).is_empty());
        assert_eq!(ended(e, db, stream)[0].end, ShareEnd::Revoked);
        rotate(e, db, stream);
        let next = live_epoch(e, db, stream).0;
        assert_eq!(grants_at(e, db, &ada, next), 0);
    }
}

/// A revocation from a device whose reads are bounded ends nothing.
#[test]
fn a_bounded_senders_revocation_ends_no_share() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&ea, &dba, &eb, &mut dbb, true);
    eb.revoke_share(&mut dbb, stream, ada.identity_id())
        .unwrap();

    bound(&mut dba, &eb);
    deliver(&eb, &dbb, &ea, &mut dba, false);
    assert_eq!(recipients(&ea, &dba, stream), vec![ada.identity_id()]);
    assert!(ended(&ea, &dba, stream).is_empty());
}

/// A device paired without `ID_S_priv`, and the founder that holds it.
fn paired_and_founder() -> (Engine, Db, Engine, Db) {
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
    (paired, db, founder, fdb)
}

/// The paired device rotates and cannot re-seal; the founder re-seals the
/// epoch once it learns it, and the paired device folds that re-seal too.
#[test]
fn a_holder_reseals_an_epoch_a_device_without_the_signing_key_minted() {
    let (paired, mut db, founder, mut fdb) = paired_and_founder();
    let stream = work_stream(&paired, &mut db);
    let ada = Recipient::new(0x31);
    deliver(&paired, &db, &founder, &mut fdb, true);
    founder
        .share_stream(&mut fdb, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&founder, &fdb, &paired, &mut db, true);
    rotate(&paired, &mut db, stream);
    rotate(&paired, &mut db, stream);
    let (minted, key) = live_epoch(&paired, &mut db, stream);
    let shared_at = shares(&paired, &db, stream)[0].epoch;
    assert!(minted > shared_at + 1, "two epochs the recipient never got");

    deliver(&paired, &db, &founder, &mut fdb, true);
    let listed = shares(&founder, &fdb, stream);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].epoch, minted,
        "the holder re-sealed the live epoch"
    );
    assert_eq!(open(&founder, &listed[0], &ada, T0), key);
    for epoch in shared_at + 1..=minted {
        assert_eq!(
            grants_at(&founder, &fdb, &ada, epoch),
            1,
            "every epoch the recipient missed reaches it once, epoch {epoch}"
        );
    }

    deliver(&founder, &fdb, &paired, &mut db, true);
    assert_eq!(shares(&paired, &db, stream)[0].epoch, minted);
    // Nothing left behind: the founder applies the paired device's ops
    // again and re-seals nothing more.
    let before = op_count(&fdb);
    deliver(&paired, &db, &founder, &mut fdb, true);
    assert_eq!(op_count(&fdb), before);
}

/// A paired device revokes one of two shares: its rotation re-seals to
/// nobody, and the founder's backfill reaches the share still in force and
/// never the revoked one.
#[test]
fn a_holders_backfill_leaves_out_a_share_the_paired_device_revoked() {
    let (paired, mut db, founder, mut fdb) = paired_and_founder();
    let stream = work_stream(&paired, &mut db);
    let (ada, bob) = (Recipient::new(0x31), Recipient::new(0x33));
    deliver(&paired, &db, &founder, &mut fdb, true);
    for r in [&ada, &bob] {
        founder
            .share_stream(&mut fdb, stream, &r.person(), r.dh.public_bytes(), None)
            .unwrap();
    }
    deliver(&founder, &fdb, &paired, &mut db, true);
    let minted = paired
        .revoke_share(&mut db, stream, ada.identity_id())
        .unwrap();
    assert_eq!(recipients(&paired, &db, stream), vec![bob.identity_id()]);

    deliver(&paired, &db, &founder, &mut fdb, true);
    assert_eq!(recipients(&founder, &fdb, stream), vec![bob.identity_id()]);
    assert_eq!(shares(&founder, &fdb, stream)[0].epoch, minted);
    assert_eq!(grants_at(&founder, &fdb, &ada, minted), 0);
    assert_eq!(grants_at(&founder, &fdb, &bob, minted), 1);
}

/// Every op `from` logged whose `inner_kind` `pick` accepts, delivered to
/// `to` in each stream's order: a relay that hands some ops over late, or
/// drops them.
pub(super) fn deliver_where(
    from: &Engine,
    from_db: &Db,
    to: &Engine,
    to_db: &mut Db,
    pick: fn(&str) -> bool,
) {
    let device = from.keychain.device_id();
    let envs: Vec<(String, Vec<u8>)> = {
        let mut stmt = from_db
            .conn()
            .prepare(
                "SELECT inner_kind, envelope FROM ops WHERE device_id = ?
                 ORDER BY stream_id, seq",
            )
            .unwrap();
        stmt.query_map(params![&device[..]], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for (kind, env) in envs {
        if pick(&kind) {
            to.apply_remote_all(to_db, &env).unwrap();
        }
    }
}

/// The paired device revokes Ada and the founder never applies the
/// revocation: the paired device's re-seal request names only Bob, so the
/// founder's backfill seals the revocation's epoch to Bob alone, though its
/// own fold still holds Ada's share in force.
#[test]
fn a_holder_that_has_not_seen_a_revocation_never_reseals_its_epoch_to_the_revoked() {
    let (paired, mut db, founder, mut fdb) = paired_and_founder();
    let stream = work_stream(&paired, &mut db);
    let (ada, bob) = (Recipient::new(0x31), Recipient::new(0x33));
    deliver(&paired, &db, &founder, &mut fdb, true);
    for r in [&ada, &bob] {
        founder
            .share_stream(&mut fdb, stream, &r.person(), r.dh.public_bytes(), None)
            .unwrap();
    }
    deliver(&founder, &fdb, &paired, &mut db, true);
    let minted = paired
        .revoke_share(&mut db, stream, ada.identity_id())
        .unwrap();

    deliver_where(&paired, &db, &founder, &mut fdb, |k| k != "share.revoke");
    assert!(
        recipients(&founder, &fdb, stream).contains(&ada.identity_id()),
        "the founder has not applied the revocation"
    );
    assert_eq!(grants_at(&founder, &fdb, &ada, minted), 0);
    assert_eq!(grants_at(&founder, &fdb, &bob, minted), 1);

    // The revocation arriving late changes nothing already sealed.
    deliver(&paired, &db, &founder, &mut fdb, true);
    assert_eq!(recipients(&founder, &fdb, stream), vec![bob.identity_id()]);
    assert_eq!(grants_at(&founder, &fdb, &ada, minted), 0);
}

/// A second founder of the account, the first founder, and a paired device,
/// each trusting the others, every one holding the first founder's vault-meta
/// key. The paired device has created a Stream every replica holds, and the
/// first founder has shared it with `ada` at its first epoch, which the paired
/// device has applied. The second founder has applied nothing of the first
/// founder's.
struct Trio {
    holder: Engine,
    hdb: Db,
    founder: Engine,
    fdb: Db,
    paired: Engine,
    pdb: Db,
    stream: EntityRef,
}

fn trio(ada: &Recipient) -> Trio {
    let (paired, mut pdb, founder, mut fdb) = paired_and_founder();
    let (holder, mut hdb) = self::founder([3u8; 32]);
    trust(&holder, &mut hdb, &founder);
    trust(&holder, &mut hdb, &paired);
    trust(&founder, &mut fdb, &holder);
    trust(&paired, &mut pdb, &holder);
    paired_meta(&holder, &mut hdb, &founder, &mut fdb);
    let stream = work_stream(&paired, &mut pdb);
    deliver(&paired, &pdb, &founder, &mut fdb, true);
    deliver(&paired, &pdb, &holder, &mut hdb, true);
    founder
        .share_stream(&mut fdb, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&founder, &fdb, &paired, &mut pdb, true);
    Trio {
        holder,
        hdb,
        founder,
        fdb,
        paired,
        pdb,
        stream,
    }
}

/// The paired device rotates; the epoch it minted, and the request naming
/// Ada, are what the second founder is to re-seal.
fn paired_rotates(t: &mut Trio) -> u32 {
    rotate(&t.paired, &mut t.pdb, t.stream);
    live_epoch(&t.paired, &mut t.pdb, t.stream).0
}

/// The second founder holds the key and the request, and the grant arrives
/// last: applying the grant is what re-seals.
#[test]
fn applying_a_share_grant_backfills_a_requested_epoch() {
    let ada = Recipient::new(0x31);
    let mut t = trio(&ada);
    let minted = paired_rotates(&mut t);
    deliver(&t.paired, &t.pdb, &t.holder, &mut t.hdb, true);
    assert_eq!(
        grants_at(&t.holder, &t.hdb, &ada, minted),
        0,
        "no share yet"
    );

    deliver(&t.founder, &t.fdb, &t.holder, &mut t.hdb, true);
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 1);
    assert_eq!(shares(&t.holder, &t.hdb, t.stream)[0].epoch, minted);
}

/// The second founder holds the grant and the request, and the key arrives
/// last, in a `KeyEnvelope`: learning it is what re-seals.
#[test]
fn learning_a_key_from_an_envelope_backfills_a_requested_epoch() {
    let ada = Recipient::new(0x31);
    let mut t = trio(&ada);
    let minted = paired_rotates(&mut t);
    deliver(&t.founder, &t.fdb, &t.holder, &mut t.hdb, true);
    deliver_where(&t.paired, &t.pdb, &t.holder, &mut t.hdb, |k| {
        k != "key.envelope"
    });
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 0, "no key yet");

    deliver(&t.paired, &t.pdb, &t.holder, &mut t.hdb, true);
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 1);
}

/// The second founder holds the grant and the request, and the key arrives
/// last, in the paired device's key bundle: absorbing it is what re-seals.
#[test]
fn learning_a_key_from_a_key_bundle_backfills_a_requested_epoch() {
    let ada = Recipient::new(0x31);
    let mut t = trio(&ada);
    let minted = paired_rotates(&mut t);
    deliver(&t.founder, &t.fdb, &t.holder, &mut t.hdb, true);
    deliver_where(&t.paired, &t.pdb, &t.holder, &mut t.hdb, |k| {
        k != "key.envelope"
    });
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 0, "no key yet");

    let plan = t
        .paired
        .key_bundle_plan(&mut t.pdb)
        .unwrap()
        .expect("the paired device publishes");
    let bundle = t.paired.seal_key_bundle(&plan).unwrap();
    t.holder.apply_key_bundle(&mut t.hdb, &bundle).unwrap();
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 1);
}

/// The second founder holds the grant and the key, and the request arrives
/// last: applying it is what re-seals.
#[test]
fn applying_a_reseal_request_backfills_its_epoch() {
    let ada = Recipient::new(0x31);
    let mut t = trio(&ada);
    let minted = paired_rotates(&mut t);
    deliver(&t.founder, &t.fdb, &t.holder, &mut t.hdb, true);
    deliver_where(&t.paired, &t.pdb, &t.holder, &mut t.hdb, |k| {
        k != "share.reseal_request"
    });
    assert_eq!(
        grants_at(&t.holder, &t.hdb, &ada, minted),
        0,
        "no request yet"
    );

    deliver(&t.paired, &t.pdb, &t.holder, &mut t.hdb, true);
    assert_eq!(grants_at(&t.holder, &t.hdb, &ada, minted), 1);
}

/// A holder's rotation re-seals its own epoch and asks nobody to: a sibling
/// holder that learns the epoch re-seals nothing more.
#[test]
fn a_holders_rotation_is_resealed_once() {
    let (ea, mut dba, eb, mut dbb) = founders();
    let stream = work_stream(&ea, &mut dba);
    let ada = Recipient::new(0x31);
    ea.share_stream(&mut dba, stream, &ada.person(), ada.dh.public_bytes(), None)
        .unwrap();
    deliver(&ea, &dba, &eb, &mut dbb, true);
    rotate(&ea, &mut dba, stream);
    let minted = live_epoch(&ea, &mut dba, stream).0;

    assert_eq!(grants_at(&ea, &dba, &ada, minted), 1, "A re-sealed");
    deliver(&ea, &dba, &eb, &mut dbb, true);
    assert_eq!(shares(&eb, &dbb, stream)[0].epoch, minted, "A's re-seal");
    let me = eb.keychain.device_id();
    let own: i64 = dbb
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE inner_kind = 'share.grant' AND device_id = ?",
            params![&me[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(own, 0, "B re-seals nothing");
}

#[test]
fn the_share_end_ops_round_trip_and_are_control_ops() {
    let revoke = InnerOp::ShareRevoke(ShareRevokePayload {
        stream_id: [1; 16],
        recipient_identity_id: [2; 16],
        effective_at_ms: 3,
        unknown: Unknowns::new(),
    });
    let decline = InnerOp::ShareDecline(ShareDeclinePayload {
        stream_id: [1; 16],
        recipient_identity_id: [2; 16],
        declined_at_ms: 3,
        unknown: Unknowns::new(),
    });
    let request = InnerOp::ShareResealRequest(ShareResealRequestPayload {
        stream_id: [1; 16],
        epoch: 2,
        key_id: [3; 8],
        recipients: vec![ResealRecipient {
            identity_id: [4; 16],
        }],
        unknown: Unknowns::new(),
    });
    for (op, kind) in [
        (revoke, "share.revoke"),
        (decline, "share.decline"),
        (request, "share.reseal_request"),
    ] {
        let bytes = encode_inner_op(&op).unwrap();
        assert_eq!(
            encode_inner_op(&decode_inner_op(&bytes).unwrap()).unwrap(),
            bytes
        );
        assert_eq!(op.inner_kind(), kind);
        assert!(op.is_control());
    }
}
