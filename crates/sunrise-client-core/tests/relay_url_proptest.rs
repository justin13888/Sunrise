//! An accepted relay origin fits the bound the rule promises
//! (`relay_url.rs`, [`MAX_RELAY_URL_LEN`]): the pairing QR's `relay_url`
//! field, which every configured relay must fit.
//!
//! The bound is easy to check on what was typed and easy to miss on what comes
//! back: IDNA serialises a non-ASCII label as `xn--…`, which can be longer
//! than the label was.

use proptest::prelude::*;
use sunrise_client_core::relay_url::{parse_relay_url, RelayUrlIssue, MAX_RELAY_URL_LEN};

/// One DNS label over ASCII and non-ASCII letters, including ones IDNA maps
/// (uppercase, fullwidth, `ß`).
fn label() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(vec![
            "a", "z", "0", "-", "A", "ü", "é", "ß", "東", "京", "ａ", "Ж", "🙂",
        ]),
        1..12,
    )
    .prop_map(|parts| parts.concat())
}

/// An `https://` or loopback `http://` URL whose host is up to 40 labels.
fn relay_url() -> impl Strategy<Value = String> {
    (
        proptest::bool::ANY,
        proptest::collection::vec(label(), 1..40),
        proptest::option::of(1u16..),
    )
        .prop_map(|(https, labels, port)| {
            let host = labels.join(".");
            let scheme = if https { "https" } else { "http" };
            let port = port.map(|p| format!(":{p}")).unwrap_or_default();
            format!("{scheme}://{host}{port}")
        })
}

proptest! {
    // `Direct`, not the `SourceParallel` default: see
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        cases: 2048,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/relay_url_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Whatever is accepted is at most `MAX_RELAY_URL_LEN` bytes as it is
    /// handed back, not only as it was typed.
    #[test]
    fn an_accepted_origin_fits_the_bound(input in relay_url()) {
        if let Ok(origin) = parse_relay_url(&input) {
            prop_assert!(
                origin.as_str().len() <= MAX_RELAY_URL_LEN,
                "{input:?} became {} bytes: {origin}",
                origin.as_str().len()
            );
        }
    }

    /// A refusal for length is `TooLong`, whichever side of IDNA it is
    /// found on, and an origin that fits is never refused as too long.
    #[test]
    fn too_long_means_the_origin_does_not_fit(input in relay_url()) {
        if parse_relay_url(&input) == Err(RelayUrlIssue::TooLong) {
            let ascii = url::Url::parse(&input).map(|u| u.origin().ascii_serialization());
            prop_assert!(
                input.len() > MAX_RELAY_URL_LEN
                    || ascii.is_ok_and(|a| a.len() > MAX_RELAY_URL_LEN),
                "{input:?} was refused as too long"
            );
        }
    }
}
