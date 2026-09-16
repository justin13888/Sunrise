//! Pairing as a dependent crate performs it: a QR, a Noise XX transcript, a
//! SAS both sides read, and then the three-message exchange — offer, request,
//! grant — across the channel.
//!
//! The unit tests in `src/` cover each piece against itself. What they cannot
//! cover, because they share a module with the encoder, is the shape of the
//! bytes that actually leave the device. Several of the tests here read the
//! encoded messages as CBOR and assert on the keys and the byte runs they
//! contain, which is the only form in which "pairing does not carry
//! `ID_S_priv`" is a statement about the wire rather than about a struct field
//! someone can add back.
//!
//! `ID_D_priv` stopped travelling in #86, which closed #76. `ID_S_priv` stopped
//! travelling here, which closes #105: while pairing handed every device the
//! account's signing key, a revoked device minted a fresh `D_S`, derived a
//! device id no revocation register had heard of, signed a genuinely valid cert
//! for it and rejoined. Neither key travels now, and the round trip is what
//! replaces the second one — the sponsor cannot sign a cert for keys the joiner
//! has not minted yet. See `docs/03-crypto/pairing-and-onboarding.md` and the
//! module docs on `protocol.rs`.

use std::collections::BTreeMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ciborium::value::{Integer, Value};
use sunrise_crypto::keys::{
    DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair,
};
use sunrise_crypto::{device_id_from_pub, identity_id_from_pub, DeviceCert, DeviceCertInner};
use sunrise_pairing::{
    account_email_hash, decode_pairing_grant, decode_pairing_offer, decode_pairing_request,
    decode_qr_payload, encode_qr_payload, PairingGrant, PairingOffer, PairingPayloadError,
    PairingRequest, PairingSession, QrPayload, Role, MAGIC_V1_HEX,
};

/// The identity signing seed. Must **not** travel, in any of the three
/// messages. This is #105.
const ID_S_PRIV: [u8; 32] = [0x21; 32];

/// The identity X25519 scalar. Must **not** travel either. This is #76.
///
/// Deliberately not a run of one repeated byte: the absence assertions below
/// scan encoded messages for this exact 32-byte window, and a low-entropy
/// pattern could plausibly appear by accident in something else.
fn id_d_priv() -> [u8; 32] {
    core::array::from_fn(|i| {
        u8::try_from(i)
            .unwrap_or(0)
            .wrapping_mul(37)
            .wrapping_add(0x9c)
    })
}

/// The joiner's own signing seed. Never leaves the joining device — only its
/// public half goes into the request.
fn d_s_priv() -> [u8; 32] {
    core::array::from_fn(|i| {
        u8::try_from(i)
            .unwrap_or(0)
            .wrapping_mul(53)
            .wrapping_add(0x41)
    })
}

/// The joiner's own DH seed, likewise.
fn d_d_priv() -> [u8; 32] {
    core::array::from_fn(|i| {
        u8::try_from(i)
            .unwrap_or(0)
            .wrapping_mul(71)
            .wrapping_add(0x17)
    })
}

/// A vault root, which travels — but only in the grant, and only after the
/// sponsor has seen a request it accepts.
const VAULT_ROOT: [u8; 32] = [0x25; 32];

const NOW_MS: u64 = 1_700_000_000_000;

fn sponsor_signing() -> IdentitySigningKeyPair {
    IdentitySigningKeyPair::from_secret_bytes(&ID_S_PRIV)
}

fn stream_keys(streams: u8, epochs: u32) -> BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> {
    let mut all = BTreeMap::new();
    for s in 0..streams {
        let mut sid = [0u8; 16];
        sid[0] = s;
        let mut per_epoch = BTreeMap::new();
        for e in 1..=epochs {
            per_epoch.insert(e, [s.wrapping_mul(19).wrapping_add(3); 32]);
        }
        all.insert(sid, per_epoch);
    }
    all
}

/// Message 1, as a sponsor with an open vault assembles it.
fn offer() -> PairingOffer {
    let id_s_pub = sponsor_signing().public_bytes();
    PairingOffer {
        id_s_pub,
        id_d_pub: IdentityDhKeyPair::from_secret_bytes(id_d_priv()).public_bytes(),
        identity_id: identity_id_from_pub(&id_s_pub),
        genesis_identity_id: identity_id_from_pub(&id_s_pub),
        genesis_id_s_pub: id_s_pub,
        nickname: "a laptop".into(),
        platform: "macos".into(),
    }
}

/// Message 2, as a joiner that has just minted its device keys assembles it.
fn request(offer: &PairingOffer) -> PairingRequest {
    let d_s = DeviceSigningKeyPair::from_secret_bytes(&d_s_priv());
    let d_d = DeviceDhKeyPair::from_secret_bytes(d_d_priv());
    PairingRequest {
        device_id: device_id_from_pub(&d_s.public_bytes()),
        d_s_pub: d_s.public_bytes(),
        d_d_pub: d_d.public_bytes(),
        identity_id: offer.identity_id,
        nickname: "a phone".into(),
        platform: "ios".into(),
    }
}

/// Message 3, as the sponsor assembles it after accepting a request.
fn grant(request: &PairingRequest, signing: &IdentitySigningKeyPair) -> PairingGrant {
    let body = DeviceCertInner {
        v: 1,
        device_id: request.device_id,
        d_s_pub: request.d_s_pub,
        d_d_pub: request.d_d_pub,
        identity_id: identity_id_from_pub(&signing.public_bytes()),
        created_at_ms: NOW_MS,
        nickname: request.nickname.clone(),
        platform: request.platform.clone(),
    };
    PairingGrant {
        device_cert: DeviceCert::issue(body, signing)
            .expect("issue")
            .to_cbor()
            .expect("cbor"),
        vault_root: VAULT_ROOT,
        stream_keys: stream_keys(3, 2),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Drive the three-message XX transcript, as a driver holding both ends would.
fn paired_channels() -> (
    sunrise_pairing::PairedChannel,
    sunrise_pairing::PairedChannel,
) {
    let n_key = PairingSession::generate_static_key().expect("static key");
    let e_key = PairingSession::generate_static_key().expect("static key");
    let mut new_device = PairingSession::new(Role::NewDevice, &n_key).expect("initiator");
    let mut existing = PairingSession::new(Role::ExistingDevice, &e_key).expect("responder");

    let m1 = new_device.write_message(&[]).expect("-> e");
    existing.read_message(&m1).expect("read e");
    let m2 = existing.write_message(&[]).expect("<- e ee s es");
    new_device.read_message(&m2).expect("read ee es");
    let m3 = new_device.write_message(&[]).expect("-> s se");
    existing.read_message(&m3).expect("read se");

    // Neither side may reach a channel without going through the SAS, and the
    // two codes must agree or the users abort.
    let shown_to_the_new_user = new_device.sas().expect("sas");
    let shown_to_the_existing_user = existing.sas().expect("sas");
    assert_eq!(shown_to_the_new_user, shown_to_the_existing_user);

    (
        new_device.into_channel(true).expect("new device confirms"),
        existing
            .into_channel(true)
            .expect("existing device confirms"),
    )
}

/// The whole flow, once, through nothing but `pub` items: the QR the new
/// device publishes, the handshake it drives, the code both users compare, the
/// offer/request/grant round trip, and the account the new device ends up
/// holding.
///
/// Note the direction of each message. The channel is bidirectional and always
/// was — the old one-shot payload simply never used the return leg.
#[test]
fn a_new_device_is_paired_end_to_end_through_the_public_api() {
    // The new device publishes a QR. `MAGIC_V1_HEX` is used rather than a
    // literal built in the test, which is what let its length be wrong once
    // with nothing noticing.
    let static_pair = PairingSession::generate_static_keypair().expect("keypair");
    let qr = QrPayload {
        magic_v1: MAGIC_V1_HEX.to_string(),
        pair_id: URL_SAFE_NO_PAD.encode([0x0e_u8; 16]),
        n_static_pub: URL_SAFE_NO_PAD.encode(&static_pair.public),
        account_email_hash: hex::encode(account_email_hash("Justin@Example.com")),
        relay_url: "https://relay.example.com".into(),
    };
    let scanned = decode_qr_payload(&encode_qr_payload(&qr).expect("encode")).expect("decode");
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(scanned.n_static_pub.as_bytes())
            .expect("base64"),
        static_pair.public,
        "the existing device must recover the exact static key from the QR"
    );

    let (mut joiner, mut sponsor) = paired_channels();

    // 1. Sponsor -> joiner.
    let sent = offer();
    let wire = sponsor.send(&sent.encode().expect("encode")).expect("seal");
    let seen = decode_pairing_offer(&joiner.receive(&wire).expect("open")).expect("decode");
    assert_eq!(seen, sent);

    // 2. Joiner mints its own device keys and asks to be certified.
    let asked = request(&seen);
    let wire = joiner.send(&asked.encode().expect("encode")).expect("seal");
    let got = decode_pairing_request(&sponsor.receive(&wire).expect("open")).expect("decode");
    assert_eq!(got, asked);
    assert_eq!(
        got.identity_id, sent.identity_id,
        "the sponsor checks that the request answers its own offer"
    );

    // 3. Sponsor issues the cert and hands over the vault.
    let issued = grant(&got, &sponsor_signing());
    let wire = sponsor
        .send(&issued.encode().expect("encode"))
        .expect("seal");
    let received = decode_pairing_grant(&joiner.receive(&wire).expect("open")).expect("decode");

    let payload = received
        .accept(&seen, d_s_priv(), d_d_priv(), &asked)
        .expect("an honest grant is accepted");
    assert_eq!(payload.id_s_pub, sent.id_s_pub);
    assert_eq!(payload.id_d_pub, sent.id_d_pub);
    assert_eq!(payload.identity_id, sent.identity_id);
    assert_eq!(payload.vault_root, VAULT_ROOT);
    assert_eq!(payload.stream_keys, stream_keys(3, 2));
    assert_eq!(payload.key_count(), 6);
    assert_eq!(payload.d_s_priv, d_s_priv());

    // The labels live in the cert, which is what the account signed, rather
    // than beside it where a second copy could disagree.
    let cert = DeviceCert::from_cbor(&payload.device_cert).expect("the granted cert parses");
    assert_eq!(cert.body.nickname, "a phone");
    assert_eq!(cert.body.platform, "ios");
}

/// **The #105 test, on the wire.** `ID_S_priv` is nowhere in any of the three
/// messages a pairing sends.
///
/// Asserted against the encoded bytes, not against the structs, because a
/// struct not having a field is a fact about today's source and the wire not
/// carrying the bytes is the property revocation now depends on. The positive
/// assertions are what make this able to fail: the vault root and every Stream
/// key *are* in the grant, so the scan demonstrably finds 32-byte secrets when
/// they are present.
#[test]
fn no_message_in_the_exchange_carries_an_identity_private_key() {
    let o = offer();
    let r = request(&o);
    let g = grant(&r, &sponsor_signing());

    let encoded = [
        ("offer", o.encode().expect("encode")),
        ("request", r.encode().expect("encode")),
        ("grant", g.encode().expect("encode")),
    ];

    // The control: the grant does carry 32-byte secrets, so the scan works.
    let grant_bytes = &encoded[2].1;
    assert!(
        contains(grant_bytes, &VAULT_ROOT),
        "the vault root travels, in the grant"
    );
    for epochs in g.stream_keys.values() {
        for key in epochs.values() {
            assert!(
                contains(grant_bytes, key),
                "every Stream key the sponsor holds travels, in the grant"
            );
        }
    }
    assert!(
        contains(&encoded[0].1, &o.id_d_pub),
        "ID_D_pub travels, in the offer; sealing to the identity needs only the public half"
    );

    for (name, bytes) in &encoded {
        assert!(
            !contains(bytes, &ID_S_PRIV),
            "ID_S_priv must not travel in the {name}: a device holding it mints a valid \
             DeviceCert for any device id it invents, which is how a revoked device \
             rejoined (#105)"
        );
        assert!(
            !contains(bytes, &id_d_priv()),
            "ID_D_priv must not travel in the {name}: a device holding it opens the \
             identity copy of every key_envelope, and no revocation can exclude it (#76)"
        );
        assert!(
            !contains(bytes, &d_s_priv()) || *name == "offer",
            "the joiner's own D_S_priv must not travel either: only its public half is asked \
             about"
        );
    }
}

/// The offer carries **no secret at all**, which is what makes an abandoned
/// pairing cost nothing.
///
/// A joiner that walks away after message 1, or an attacker that intercepts
/// only that one, has learned nothing the account has not already published.
/// The vault root and the Stream keys do not move until the sponsor has seen a
/// request it accepts.
#[test]
fn an_abandoned_pairing_leaks_nothing_because_the_offer_holds_nothing() {
    let bytes = offer().encode().expect("encode");
    assert!(!contains(&bytes, &VAULT_ROOT), "no vault root in an offer");
    assert!(!contains(&bytes, &ID_S_PRIV), "no ID_S_priv in an offer");
    assert!(!contains(&bytes, &id_d_priv()), "no ID_D_priv in an offer");
    for epochs in stream_keys(3, 2).values() {
        for key in epochs.values() {
            assert!(!contains(&bytes, key), "no Stream key in an offer");
        }
    }
}

/// Each message's wire map holds exactly the fields this version defines.
///
/// A separate assertion from the byte scans above, and not a redundant one: a
/// scan would still pass if a burned field came back carrying a *different*
/// 32-byte value, and this would not.
#[test]
fn each_wire_map_carries_exactly_the_fields_this_version_defines() {
    let keys_of = |bytes: &[u8]| -> Vec<i128> {
        let Value::Map(entries) =
            ciborium::de::from_reader::<Value, _>(bytes).expect("a pairing message is CBOR")
        else {
            panic!("a pairing message is a map");
        };
        let mut keys: Vec<i128> = entries
            .iter()
            .map(|(k, _)| match k {
                Value::Integer(i) => i128::from(*i),
                other => panic!("every key is an integer, got {other:?}"),
            })
            .collect();
        keys.sort_unstable();
        keys
    };

    let o = offer();
    let r = request(&o);
    let g = grant(&r, &sponsor_signing());
    assert_eq!(
        keys_of(&o.encode().expect("encode")),
        vec![1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        keys_of(&r.encode().expect("encode")),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(keys_of(&g.encode().expect("encode")), vec![1, 2, 3]);
}

/// Build an offer map by hand, the way a sender that is not this encoder would.
/// `extra` is appended after the fields every version defines.
fn hand_built_offer(extra: &[(u8, Value)]) -> Vec<u8> {
    let o = offer();
    let mut map = vec![
        (
            Value::Integer(Integer::from(1_u8)),
            Value::Bytes(o.id_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(2_u8)),
            Value::Bytes(o.id_d_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3_u8)),
            Value::Bytes(o.identity_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4_u8)),
            Value::Bytes(o.genesis_identity_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(5_u8)),
            Value::Bytes(o.genesis_id_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(6_u8)),
            Value::Text(o.nickname.clone()),
        ),
        (
            Value::Integer(Integer::from(7_u8)),
            Value::Text(o.platform.clone()),
        ),
    ];
    for (k, v) in extra {
        map.push((Value::Integer(Integer::from(*k)), v.clone()));
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("encode");
    out
}

/// A sender that puts the wrong CBOR type in a field this version defines is
/// refused; a *newer* sender's unknown field is ignored.
///
/// Two halves of one decision. Nothing in an offer is signed, so a field this
/// build does not understand costs nothing to drop — which is exactly why the
/// reserved range has to be explicit about which numbers are not droppable.
///
/// The first assertion is the control: it proves the hand-built fixture is
/// otherwise acceptable, so the refusal is attributable to the field and not to
/// a malformed map.
#[test]
fn the_reserved_range_refuses_a_known_field_and_the_ignore_arm_accepts_a_new_one() {
    let honest = hand_built_offer(&[]);
    assert_eq!(
        decode_pairing_offer(&honest).expect("a hand-built honest offer decodes"),
        offer()
    );

    // Field 3 is `identity_id`, a 16-byte string. A sender emitting text there
    // is broken, and "broken" must not decode to a default.
    let wrong_shape = {
        let mut out = Vec::new();
        let Value::Map(mut map) =
            ciborium::de::from_reader::<Value, _>(honest.as_slice()).expect("decode")
        else {
            unreachable!()
        };
        map.retain(|(k, _)| !matches!(k, Value::Integer(i) if i128::from(*i) == 3));
        map.push((
            Value::Integer(Integer::from(3_u8)),
            Value::Text("not an id".into()),
        ));
        ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("encode");
        out
    };
    assert!(matches!(
        decode_pairing_offer(&wrong_shape),
        Err(PairingPayloadError::BadField("offer field shape"))
    ));

    // 8, not 7: the offer defines 1..=7, so the first field a newer sender
    // could add is the one after them. A test that reused a defined number
    // would be asserting that a field this version *understands* is ignorable,
    // which is the opposite of what the reserved range exists for.
    let from_the_future = hand_built_offer(&[(8, Value::Text("something new".into()))]);
    assert_eq!(
        decode_pairing_offer(&from_the_future).expect("a newer sender must still pair"),
        offer()
    );
}

/// A frame altered in flight yields nothing at all — not a partial or corrupt
/// message the new device might act on.
#[test]
fn a_message_tampered_with_in_flight_never_reaches_the_new_device() {
    let (mut joiner, mut sponsor) = paired_channels();
    let mut wire = sponsor
        .send(&offer().encode().expect("encode"))
        .expect("seal");

    // A relay is the only thing between the two devices, and it is not trusted.
    let midpoint = wire.len() / 2;
    wire[midpoint] ^= 0x20;

    assert!(
        joiner.receive(&wire).is_err(),
        "the AEAD must reject the frame rather than surface damaged plaintext"
    );
}

/// The tamper Noise cannot see: the sender itself is dishonest.
///
/// A modified frame fails the AEAD, so the interesting adversary is on the
/// authenticated end of the channel — a device sealing an offer whose
/// `identity_id` names one account while its `ID_S_pub` belongs to another.
/// Nothing about the transport can catch that; only the decoder's own
/// recomputation can, and it is the check that decides whose account this
/// device joins.
#[test]
fn an_offer_naming_an_identity_its_signing_key_does_not_derive_is_refused() {
    let (mut joiner, mut sponsor) = paired_channels();

    let mut dishonest = offer();
    dishonest.identity_id = [0x88; 16];
    let wire = sponsor
        .send(&dishonest.encode().expect("encode"))
        .expect("seal");

    // The channel delivers it intact — this is a real message from the peer
    // the SAS confirmed.
    let plaintext = joiner
        .receive(&wire)
        .expect("the frame is authentic; the tamper is in its contents");
    assert!(
        matches!(
            decode_pairing_offer(&plaintext),
            Err(PairingPayloadError::IdentityMismatch)
        ),
        "the receiver must recompute identity_id rather than trust it"
    );
}

/// **The replay test.** A request captured from one pairing and presented to a
/// *second* sponsor gets a cert the joiner refuses.
///
/// The second sponsor is a different account, so it signs with a different
/// `ID_S_priv`. It has no way to know the request is not addressed to it — a
/// request is public material and carries no secret — so it issues. The check
/// that matters is on the joiner, which holds the first sponsor's offer and
/// compares the cert against *that* `ID_S_pub`.
///
/// This is why the joiner verifies against the offer's key rather than against
/// whatever identity the cert names: a cert that is internally consistent under
/// some other well-formed identity would otherwise pass.
#[test]
fn a_request_replayed_at_a_second_sponsor_yields_a_cert_the_joiner_refuses() {
    let first = offer();
    let asked = request(&first);

    // The second sponsor: a real account, a real identity, no relationship to
    // the first. It issues a perfectly valid cert of its own.
    let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x77; 32]);
    let replayed = grant(&asked, &stranger);
    assert!(
        DeviceCert::from_cbor(&replayed.device_cert)
            .expect("parses")
            .verify(&stranger.public_bytes())
            .is_ok(),
        "the stranger's cert is genuinely valid under the stranger's identity — \
         which is exactly why the joiner cannot judge it on its own terms"
    );

    let err = replayed
        .accept(&first, d_s_priv(), d_d_priv(), &asked)
        .expect_err("a cert from an identity the offer did not name must be refused");
    assert!(matches!(err, PairingPayloadError::BadCert(_)));
}

/// **The #152 test.** The static key the responder ends up talking to is the
/// one the QR published — and it is a *substituted* one when a third party
/// drives the transcript instead.
///
/// This is the assertion `qr.rs` describes as the QR path's authentication, and
/// until `PairingSession::peer_static_key` existed it could not be made from
/// outside the crate at all: Noise XX carries the initiator's static in the
/// third message, `snow`'s `get_remote_static` is not re-exported, and nothing
/// on `PairingSession` surfaced it. A dependent could drive the whole flow and
/// still not check the one value the user transferred out of band.
///
/// Note what fails and what does not. The impostor's transcript **completes**,
/// and both its sides read a SAS; Noise authenticates that the two ends of a
/// transcript agree, not that either is who a QR said it would be. The QR is
/// the only thing that can tell them apart, and only if someone compares it.
#[test]
fn the_responder_can_check_the_peer_static_against_the_qr_it_scanned() {
    // The new device publishes a QR carrying the public half of the static key
    // it will drive the handshake with.
    let published = PairingSession::generate_static_keypair().expect("keypair");
    let qr = QrPayload {
        magic_v1: MAGIC_V1_HEX.to_string(),
        pair_id: URL_SAFE_NO_PAD.encode([0x0e_u8; 16]),
        n_static_pub: URL_SAFE_NO_PAD.encode(&published.public),
        account_email_hash: hex::encode(account_email_hash("Justin@Example.com")),
        relay_url: "https://relay.example.com".into(),
    };
    let scanned = decode_qr_payload(&encode_qr_payload(&qr).expect("encode")).expect("decode");
    let from_the_qr = URL_SAFE_NO_PAD
        .decode(scanned.n_static_pub.as_bytes())
        .expect("base64");

    let run = |initiator_key: &[u8]| -> Option<Vec<u8>> {
        let e_key = PairingSession::generate_static_key().expect("static key");
        let mut new_device =
            PairingSession::new(Role::NewDevice, initiator_key).expect("initiator");
        let mut existing = PairingSession::new(Role::ExistingDevice, &e_key).expect("responder");

        // Before the third message the responder has learned nothing about
        // the initiator: XX defers identity, which is why the check cannot be
        // made any earlier than completion.
        let m1 = new_device.write_message(&[]).expect("-> e");
        existing.read_message(&m1).expect("read e");
        assert!(
            existing.peer_static_key().is_none(),
            "the responder must not claim a peer static before XX carries one"
        );
        let m2 = existing.write_message(&[]).expect("<- e ee s es");
        new_device.read_message(&m2).expect("read ee es");
        let m3 = new_device.write_message(&[]).expect("-> s se");
        existing.read_message(&m3).expect("read se");

        assert!(existing.is_complete());
        // The transcript completes, and a SAS exists, whoever drove it.
        assert_eq!(
            existing.sas().expect("sas"),
            new_device.sas().expect("sas"),
            "both ends of any completed transcript read the same code"
        );
        existing.peer_static_key()
    };

    assert_eq!(
        run(&published.private).as_deref(),
        Some(from_the_qr.as_slice()),
        "the honest new device's static must be the one the QR carried"
    );

    let impostor = PairingSession::generate_static_keypair().expect("keypair");
    assert_ne!(
        run(&impostor.private).as_deref(),
        Some(from_the_qr.as_slice()),
        "a substituted static must not pass for the QR's, or the QR authenticates nothing"
    );
}

/// A consumed handshake reports no peer static.
///
/// `into_channel` takes the `HandshakeState`, and an accessor that kept
/// answering afterwards would be reading a key out of a session whose material
/// is supposed to be gone.
#[test]
fn a_consumed_session_has_no_peer_static_to_report() {
    let n_key = PairingSession::generate_static_key().expect("static key");
    let e_key = PairingSession::generate_static_key().expect("static key");
    let mut new_device = PairingSession::new(Role::NewDevice, &n_key).expect("initiator");
    let mut existing = PairingSession::new(Role::ExistingDevice, &e_key).expect("responder");

    let m1 = new_device.write_message(&[]).expect("-> e");
    existing.read_message(&m1).expect("read e");
    let m2 = existing.write_message(&[]).expect("<- e ee s es");
    new_device.read_message(&m2).expect("read ee es");
    let m3 = new_device.write_message(&[]).expect("-> s se");
    existing.read_message(&m3).expect("read se");

    assert!(existing.peer_static_key().is_some());
    let _channel = existing.into_channel(true).expect("confirm");
    // `existing` is consumed; the new device's own view is the one still open,
    // and it too reports a peer static until it is consumed.
    assert!(new_device.peer_static_key().is_some());
    let _other = new_device.into_channel(true).expect("confirm");
}
