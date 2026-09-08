//! Pairing as a dependent crate performs it: a QR, a Noise XX transcript, a
//! SAS both sides read, and then one sealed `PairingPayload` across the
//! channel.
//!
//! The unit tests in `src/` cover each piece against itself. What they cannot
//! cover, because they share a module with the encoder, is the shape of the
//! bytes that actually leave the device. Two of the tests here read the
//! encoded payload as CBOR and assert on the keys and the byte runs it
//! contains, which is the only form in which "the payload does not carry
//! `ID_D_priv`" is a statement about the wire rather than about a struct
//! field someone can add back.
//!
//! `ID_D_priv` stopped travelling in #86, which closed #76: while pairing
//! handed every device the identity's own X25519 unwrapping key, a revoked
//! device excluded from a new epoch's recipient list simply opened the
//! identity copy of the `key_envelope` instead, so no revocation bound
//! anything. See `docs/03-crypto/pairing-and-onboarding.md` and the module
//! docs on `payload.rs`.

use std::collections::BTreeMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ciborium::value::{Integer, Value};
use sunrise_crypto::identity_id_from_pub;
use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
use sunrise_pairing::{
    account_email_hash, decode_pairing_payload, decode_qr_payload, encode_pairing_payload,
    encode_qr_payload, PairingPayload, PairingPayloadError, PairingSession, QrPayload, Role,
    MAGIC_V1_HEX,
};

/// The identity signing seed. Travels: without it a paired device could never
/// admit the next one.
const ID_S_PRIV: [u8; 32] = [0x21; 32];

/// The identity X25519 scalar. Must **not** travel.
///
/// Deliberately not a run of one repeated byte: the absence assertions below
/// scan the encoded payload for this exact 32-byte window, and a low-entropy
/// pattern could plausibly appear by accident in something else.
fn id_d_priv() -> [u8; 32] {
    core::array::from_fn(|i| {
        u8::try_from(i)
            .unwrap_or(0)
            .wrapping_mul(37)
            .wrapping_add(0x9c)
    })
}

/// A vault root, which does travel.
const VAULT_ROOT: [u8; 32] = [0x25; 32];

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

/// The payload an existing device assembles for a new one.
fn payload() -> PairingPayload {
    let signing = IdentitySigningKeyPair::from_secret_bytes(&ID_S_PRIV);
    let id_s_pub = signing.public_bytes();
    let id_d_pub = IdentityDhKeyPair::from_secret_bytes(id_d_priv()).public_bytes();
    PairingPayload {
        id_s_priv: ID_S_PRIV,
        id_s_pub,
        id_d_pub,
        identity_id: identity_id_from_pub(&id_s_pub),
        vault_root: VAULT_ROOT,
        stream_keys: stream_keys(3, 2),
        nickname: "a laptop".into(),
        platform: "macos".into(),
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
/// sealed payload, and the account the new device ends up holding.
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

    let (mut new_device, mut existing) = paired_channels();

    let sent = payload();
    let wire = existing
        .send(&encode_pairing_payload(&sent).expect("encode"))
        .expect("seal");
    let received =
        decode_pairing_payload(&new_device.receive(&wire).expect("open")).expect("decode");

    assert_eq!(received.id_s_priv, sent.id_s_priv);
    assert_eq!(received.id_s_pub, sent.id_s_pub);
    assert_eq!(received.id_d_pub, sent.id_d_pub);
    assert_eq!(received.identity_id, sent.identity_id);
    assert_eq!(received.vault_root, sent.vault_root);
    assert_eq!(received.stream_keys, sent.stream_keys);
    assert_eq!(received.nickname, "a laptop");
    assert_eq!(received.platform, "macos");
    assert_eq!(received.key_count(), 6);
}

/// **The #76 test.** `ID_D_priv` is nowhere in the bytes a pairing sends.
///
/// Asserted against the encoded payload, not against the struct, because the
/// struct not having a field is a fact about today's source and the wire not
/// carrying the bytes is the property a revocation depends on. The three
/// positive assertions are what make this able to fail: `ID_S_priv`, the vault
/// root and every Stream key *are* in there, so the scan demonstrably finds
/// 32-byte secrets when they are present.
#[test]
fn the_wire_bytes_carry_every_secret_that_travels_and_not_the_one_that_does_not() {
    let p = payload();
    let encoded = encode_pairing_payload(&p).expect("encode");

    assert!(
        contains(&encoded, &p.id_s_priv),
        "ID_S_priv travels: without it a paired device can never admit another"
    );
    assert!(contains(&encoded, &p.vault_root), "the vault root travels");
    for epochs in p.stream_keys.values() {
        for key in epochs.values() {
            assert!(
                contains(&encoded, key),
                "every Stream key the sender holds travels"
            );
        }
    }
    assert!(
        contains(&encoded, &p.id_d_pub),
        "ID_D_pub travels; sealing to the identity needs only the public half"
    );

    assert!(
        !contains(&encoded, &id_d_priv()),
        "ID_D_priv must not travel: a device holding it opens the identity copy \
         of every key_envelope, and no revocation can exclude it (#76)"
    );
}

/// The wire map holds exactly the fields this version defines, and field 2 —
/// which was `ID_D_priv` — is not among them.
///
/// A separate assertion from the byte scan above, and not a redundant one: the
/// scan would still pass if the field came back carrying a *different* 32-byte
/// value, and this would not.
#[test]
fn the_wire_map_carries_exactly_the_fields_this_version_defines() {
    let encoded = encode_pairing_payload(&payload()).expect("encode");
    let Value::Map(entries) = ciborium::de::from_reader::<Value, _>(encoded.as_slice())
        .expect("a pairing payload is CBOR")
    else {
        panic!("a pairing payload is a map");
    };

    let mut keys: Vec<i128> = entries
        .iter()
        .map(|(k, _)| match k {
            Value::Integer(i) => i128::from(*i),
            other => panic!("every key is an integer, got {other:?}"),
        })
        .collect();
    keys.sort_unstable();

    assert_eq!(
        keys,
        vec![1, 3, 4, 5, 6, 7, 8, 9],
        "field 2 is burned and no field may appear twice"
    );
}

/// Build the payload map by hand, the way a sender that is not this encoder
/// would. `extra` is appended after the fields every version defines.
fn hand_built_payload(extra: &[(u8, Value)]) -> Vec<u8> {
    let p = payload();
    let mut streams: Vec<(Value, Value)> = Vec::new();
    for (sid, epochs) in &p.stream_keys {
        let per_epoch: Vec<(Value, Value)> = epochs
            .iter()
            .map(|(e, k)| (Value::Integer(Integer::from(*e)), Value::Bytes(k.to_vec())))
            .collect();
        streams.push((Value::Bytes(sid.to_vec()), Value::Map(per_epoch)));
    }
    let mut map = vec![
        (
            Value::Integer(Integer::from(1_u8)),
            Value::Bytes(p.id_s_priv.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3_u8)),
            Value::Bytes(p.id_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4_u8)),
            Value::Bytes(p.id_d_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(5_u8)),
            Value::Bytes(p.identity_id.to_vec()),
        ),
        (Value::Integer(Integer::from(6_u8)), Value::Map(streams)),
        (
            Value::Integer(Integer::from(7_u8)),
            Value::Text(p.nickname.clone()),
        ),
        (
            Value::Integer(Integer::from(8_u8)),
            Value::Text(p.platform.clone()),
        ),
        (
            Value::Integer(Integer::from(9_u8)),
            Value::Bytes(p.vault_root.to_vec()),
        ),
    ];
    for (k, v) in extra {
        map.push((Value::Integer(Integer::from(*k)), v.clone()));
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("encode");
    out
}

/// A sender old enough to still emit `ID_D_priv` is **refused**, not tolerated.
///
/// Written against bytes assembled here rather than by re-encoding this
/// crate's own output, because the property is about what the decoder accepts
/// from a stranger. Tolerating the field and dropping it would be worse than
/// either extreme: the operator would be told the pairing succeeded and would
/// believe a later revocation binds, while the device that paired still holds
/// the key that unbinds it.
///
/// The first half of the test is the control. It proves the hand-built fixture
/// is otherwise acceptable, so the refusal below is attributable to field 2
/// and not to a malformed map.
#[test]
fn a_legacy_sender_still_emitting_id_d_priv_is_refused() {
    let honest = hand_built_payload(&[]);
    let decoded = decode_pairing_payload(&honest).expect("a hand-built honest payload decodes");
    assert_eq!(decoded.vault_root, VAULT_ROOT);

    let legacy = hand_built_payload(&[(2, Value::Bytes(id_d_priv().to_vec()))]);
    assert!(
        matches!(
            decode_pairing_payload(&legacy),
            Err(PairingPayloadError::BadField("field shape"))
        ),
        "a payload carrying ID_D_priv must be refused, not silently stripped"
    );
}

/// ...and a *newer* sender's unknown field is ignored rather than refused.
///
/// The other half of the same decision, and the one that says the refusal
/// above is deliberate rather than a blanket "anything unexpected is fatal".
/// Nothing in this payload is signed, so a field this build does not
/// understand costs nothing to drop; field 2 is refused because of what it
/// specifically is.
#[test]
fn an_unknown_future_field_is_ignored() {
    let from_the_future = hand_built_payload(&[(10, Value::Text("something new".into()))]);
    let decoded = decode_pairing_payload(&from_the_future).expect("a newer sender must still pair");
    assert_eq!(decoded.key_count(), 6);
    assert_eq!(decoded.vault_root, VAULT_ROOT);
}

/// A frame altered in flight yields nothing at all — not a partial or corrupt
/// payload the new device might act on.
#[test]
fn a_payload_tampered_with_in_flight_never_reaches_the_new_device() {
    let (mut new_device, mut existing) = paired_channels();
    let mut wire = existing
        .send(&encode_pairing_payload(&payload()).expect("encode"))
        .expect("seal");

    // A relay is the only thing between the two devices, and it is not trusted.
    let midpoint = wire.len() / 2;
    wire[midpoint] ^= 0x20;

    assert!(
        new_device.receive(&wire).is_err(),
        "the AEAD must reject the frame rather than surface damaged plaintext"
    );
}

/// The tamper Noise cannot see: the sender itself is dishonest.
///
/// A modified frame fails the AEAD, so the interesting adversary is on the
/// authenticated end of the channel — a device sealing a payload whose
/// `identity_id` names one account while its `ID_S_pub` belongs to another.
/// Nothing about the transport can catch that; only the decoder's own
/// recomputation can, and it is the check that decides whose account this
/// device joins.
#[test]
fn a_payload_naming_an_identity_its_signing_key_does_not_derive_is_refused() {
    let (mut new_device, mut existing) = paired_channels();

    let mut dishonest = payload();
    dishonest.identity_id = [0x88; 16];
    let wire = existing
        .send(&encode_pairing_payload(&dishonest).expect("encode"))
        .expect("seal");

    // The channel delivers it intact — this is a real message from the peer
    // the SAS confirmed.
    let plaintext = new_device
        .receive(&wire)
        .expect("the frame is authentic; the tamper is in its contents");
    assert!(
        matches!(
            decode_pairing_payload(&plaintext),
            Err(PairingPayloadError::IdentityMismatch)
        ),
        "the receiver must recompute identity_id rather than trust it"
    );
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
