//! The five pairing decoders that read bytes from the other device or from a
//! camera: [`decode_pairing_offer`], [`decode_pairing_request`],
//! [`decode_pairing_grant`], [`decode_pairing_payload`] and
//! [`decode_qr_payload`].
//!
//! Every input goes through all five. Four of them share one CBOR front end
//! and differ only in which integer keys they read, so a seed of one message
//! is a near miss for the other three, and mutating it explores each
//! decoder's handling of the others' fields. The QR decoder reads the input
//! only when it is UTF-8, the form a scanned code reaches it in.
//!
//! Beyond not panicking, whatever a decoder accepts survives its own encoder:
//! encoding the decoded value succeeds, decoding that encoding yields the same
//! value, and encoding again yields the same bytes. Byte identity with the
//! *input* is not the property, because each decoder skips unknown keys for
//! forward compatibility and none of them demands canonical input; what is
//! asserted is that nothing a decoder hands back is lost or changed by the
//! round trip a relay or a store would put it through.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_pairing::{
    decode_pairing_grant, decode_pairing_offer, decode_pairing_payload, decode_pairing_request,
    decode_qr_payload, encode_pairing_payload, encode_qr_payload, PairingGrant, PairingPayload,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(offer) = decode_pairing_offer(data) {
        let bytes = offer.encode().expect("a decoded offer encodes");
        let back = decode_pairing_offer(&bytes).expect("an encoded offer decodes");
        assert_eq!(back, offer, "the offer changed in a round trip");
        assert_eq!(back.encode().expect("re-encode"), bytes);
    }

    if let Ok(request) = decode_pairing_request(data) {
        let bytes = request.encode().expect("a decoded request encodes");
        let back = decode_pairing_request(&bytes).expect("an encoded request decodes");
        assert_eq!(back, request, "the request changed in a round trip");
        assert_eq!(back.encode().expect("re-encode"), bytes);
    }

    if let Ok(grant) = decode_pairing_grant(data) {
        let bytes = grant.encode().expect("a decoded grant encodes");
        let back = decode_pairing_grant(&bytes).expect("an encoded grant decodes");
        assert_same_grant(&back, &grant);
        assert_eq!(back.encode().expect("re-encode"), bytes);
    }

    if let Ok(payload) = decode_pairing_payload(data) {
        let bytes = encode_pairing_payload(&payload).expect("a decoded payload encodes");
        let back = decode_pairing_payload(&bytes).expect("an encoded payload decodes");
        assert_same_payload(&back, &payload);
        assert_eq!(encode_pairing_payload(&back).expect("re-encode"), bytes);
    }

    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(qr) = decode_qr_payload(text) {
            let json = encode_qr_payload(&qr).expect("a decoded QR payload encodes");
            let back = decode_qr_payload(&json).expect("an encoded QR payload decodes");
            assert_eq!(back, qr, "the QR payload changed in a round trip");
            assert_eq!(encode_qr_payload(&back).expect("re-encode"), json);
        }
    }
});

/// [`PairingGrant`] has no `PartialEq` (it holds key material and compares
/// nothing), so the fields are compared one by one. Every field is public;
/// a field added later without a line here fails the exhaustive destructure.
fn assert_same_grant(a: &PairingGrant, b: &PairingGrant) {
    let PairingGrant {
        device_cert,
        vault_root,
        stream_keys,
        read_bounds,
    } = a;
    assert_eq!(device_cert, &b.device_cert, "grant device_cert");
    assert_eq!(vault_root, &b.vault_root, "grant vault_root");
    assert_eq!(stream_keys, &b.stream_keys, "grant stream_keys");
    assert_eq!(read_bounds, &b.read_bounds, "grant read_bounds");
}

/// As [`assert_same_grant`], for [`PairingPayload`].
fn assert_same_payload(a: &PairingPayload, b: &PairingPayload) {
    let PairingPayload {
        id_s_pub,
        id_d_pub,
        identity_id,
        genesis_identity_id,
        genesis_id_s_pub,
        d_s_priv,
        d_d_priv,
        device_cert,
        vault_root,
        stream_keys,
        read_bounds,
    } = a;
    assert_eq!(id_s_pub, &b.id_s_pub, "payload id_s_pub");
    assert_eq!(id_d_pub, &b.id_d_pub, "payload id_d_pub");
    assert_eq!(identity_id, &b.identity_id, "payload identity_id");
    assert_eq!(
        genesis_identity_id, &b.genesis_identity_id,
        "payload genesis_identity_id"
    );
    assert_eq!(
        genesis_id_s_pub, &b.genesis_id_s_pub,
        "payload genesis_id_s_pub"
    );
    assert_eq!(d_s_priv, &b.d_s_priv, "payload d_s_priv");
    assert_eq!(d_d_priv, &b.d_d_priv, "payload d_d_priv");
    assert_eq!(device_cert, &b.device_cert, "payload device_cert");
    assert_eq!(vault_root, &b.vault_root, "payload vault_root");
    assert_eq!(stream_keys, &b.stream_keys, "payload stream_keys");
    assert_eq!(read_bounds, &b.read_bounds, "payload read_bounds");
}
