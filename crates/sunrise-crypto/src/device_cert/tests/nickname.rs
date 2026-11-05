//! The nickname bound.
//!
//! `tstr .size (1..64)` makes 64 the longest admissible nickname, and both
//! `body_to_cbor` and `body_from_cbor` spell that as `len() > 64`. A nickname
//! of exactly 64 bytes is the single input where `>` and `>=` disagree, so it
//! is the only input that pins the comparison.

use super::*;

#[test]
fn nickname_too_long_rejected() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let mut body = fixture([1u8; 32]);
    body.nickname = "x".repeat(65);
    assert!(matches!(
        DeviceCert::issue(body, &identity),
        Err(DeviceCertError::Validation(_))
    ));
}

#[test]
fn a_nickname_of_exactly_64_bytes_encodes() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let mut body = fixture([1u8; 32]);
    body.nickname = "x".repeat(64);
    assert_eq!(body.nickname.len(), 64, "the boundary this test exists for");

    let cert = DeviceCert::issue(body.clone(), &identity)
        .expect("64 bytes is inside the bound, not outside it");
    let back = DeviceCert::from_cbor(&cert.to_cbor().expect("encodes")).expect("decodes");
    assert_eq!(back.body.nickname, body.nickname);
    back.verify(&identity.public_bytes())
        .expect("a longest-legal nickname still verifies");
}

#[test]
fn a_nickname_of_exactly_64_bytes_decodes() {
    let nickname = "x".repeat(64);
    let bytes = body_cbor_with(7, Value::Text(nickname.clone()));
    assert_eq!(
        body_from_cbor(&bytes)
            .expect("64 bytes is inside the bound")
            .nickname,
        nickname
    );
}

#[test]
fn a_nickname_of_65_bytes_is_refused_by_the_decoder() {
    let bytes = body_cbor_with(7, Value::Text("x".repeat(65)));
    assert!(matches!(
        body_from_cbor(&bytes),
        Err(DeviceCertError::Validation("nickname length"))
    ));
}

// The bound is on BYTES, not characters. With ASCII nicknames the two
// readings are indistinguishable, so `"x".repeat(n)` alone leaves
// `len()` silently reinterpretable as `chars().count()`. A three-byte
// character separates them: 22 of them are 66 bytes but only 22
// characters, so the byte reading refuses what the character reading
// waves through. Getting this wrong is not academic — `sunrise-server`
// refuses a nickname over 64 *bytes*
// (`crates/sunrise-server/src/api/devices.rs`, `MAX_NICKNAME_BYTES`), so a
// character bound here would mint a validly signed cert on the pairing
// path that the service then rejects.
/// U+65E5, three bytes in UTF-8.
const MULTIBYTE: &str = "\u{65e5}";

#[test]
fn a_multibyte_nickname_of_63_bytes_is_accepted() {
    let nickname = MULTIBYTE.repeat(21);
    assert_eq!((nickname.len(), nickname.chars().count()), (63, 21));

    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let mut body = fixture([1u8; 32]);
    body.nickname = nickname.clone();
    let cert = DeviceCert::issue(body, &identity).expect("63 bytes is inside the bound");
    let back = DeviceCert::from_cbor(&cert.to_cbor().expect("encodes")).expect("decodes");
    assert_eq!(back.body.nickname, nickname);

    assert_eq!(
        body_from_cbor(&body_cbor_with(7, Value::Text(nickname.clone())))
            .expect("63 bytes is inside the bound")
            .nickname,
        nickname
    );
}

/// The falsifier for the unit: 66 bytes but only 22 characters, so this is
/// refused under the byte bound the CDDL and `sunrise-server` both state,
/// and accepted under a character bound.
#[test]
fn a_multibyte_nickname_of_66_bytes_is_refused_though_it_is_only_22_characters() {
    let nickname = MULTIBYTE.repeat(22);
    assert_eq!((nickname.len(), nickname.chars().count()), (66, 22));
    assert!(
        nickname.chars().count() <= 64,
        "the character count must stay inside the bound, or this proves nothing"
    );

    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let mut body = fixture([1u8; 32]);
    body.nickname = nickname.clone();
    assert!(
        matches!(
            DeviceCert::issue(body, &identity),
            Err(DeviceCertError::Validation("nickname length"))
        ),
        "body_to_cbor must bound the nickname in bytes, not characters"
    );

    assert!(
        matches!(
            body_from_cbor(&body_cbor_with(7, Value::Text(nickname))),
            Err(DeviceCertError::Validation("nickname length"))
        ),
        "body_from_cbor must bound the nickname in bytes, not characters"
    );
}

/// The encoder half of the bound's lower end. Without it `DeviceCert::issue`
/// mints and signs a cert that its own decoder refuses — a valid-looking
/// artefact no peer can read, discovered only at read time on the pairing
/// path.
#[test]
fn an_empty_nickname_is_refused_by_the_encoder() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let mut body = fixture([1u8; 32]);
    body.nickname = String::new();
    assert!(matches!(
        DeviceCert::issue(body, &identity),
        Err(DeviceCertError::Validation("nickname length"))
    ));
}

#[test]
fn an_empty_nickname_is_refused_by_the_decoder() {
    let bytes = body_cbor_with(7, Value::Text(String::new()));
    assert!(matches!(
        body_from_cbor(&bytes),
        Err(DeviceCertError::Validation("nickname length"))
    ));
}
