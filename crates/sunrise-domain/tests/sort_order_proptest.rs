//! The properties `sort_order` rests on, over arbitrary bounds rather than the
//! fixed walk and literals its unit tests use: for any two keys, string order
//! is numeric order, and `between` and `spread` land strictly inside them.

use proptest::prelude::*;
use std::cmp::Ordering;
use sunrise_domain::sort_order::{append_after, between, is_valid, spread, SortOrderError};

/// Well-formed digits, weighted toward the alphabet's ends: zeros make
/// trailing-zero spellings and top digits force the adjacent-digit walk.
fn digits(max: usize) -> impl Strategy<Value = String> {
    let digit = prop_oneof![
        3 => Just(b'A'),
        2 => Just(b'B'),
        2 => Just(b'Y'),
        3 => Just(b'Z'),
        2 => b'A'..=b'Z',
    ];
    proptest::collection::vec(digit, 0..=max)
        .prop_map(|d| String::from_utf8(d).expect("ASCII digits"))
}

/// Two digit strings of any two lengths sharing a prefix, so the walk has
/// to copy digits before the bounds differ. Either may have trailing zeros
/// and either order may come out, so some pairs are not increasing.
fn bounds() -> impl Strategy<Value = (String, String)> {
    (digits(4), digits(6), digits(6)).prop_map(|(p, a, b)| (format!("{p}{a}"), format!("{p}{b}")))
}

/// A valid key: any digits, then a last digit that is not zero.
fn key() -> impl Strategy<Value = String> {
    (digits(6), b'B'..=b'Z').prop_map(|(d, last)| format!("{d}{}", last as char))
}

/// The bounds as base-26 fractions, computed with no reference to the
/// code under test: the shorter is padded with the zero digit.
fn numeric(a: &str, b: &str) -> Ordering {
    let width = a.len().max(b.len());
    let pad = |k: &str| -> Vec<u8> {
        k.bytes()
            .chain(std::iter::repeat(b'A'))
            .take(width)
            .collect()
    };
    pad(a).cmp(&pad(b))
}

proptest! {
    // `Direct`, not the `SourceParallel` default: nothing above a `tests/`
    // file holds a `lib.rs` or `main.rs`. See docs/10-cross-cutting/testing.md
    // section 2.
    #![proptest_config(ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/sort_order_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// `ORDER BY sort_order` is the numeric order the module reasons in.
    #[test]
    fn string_order_is_numeric_order_for_any_two_keys(
        (p, a, b) in (digits(3), key(), key()),
    ) {
        let (a, b) = (format!("{p}{a}"), format!("{p}{b}"));
        prop_assert!(is_valid(&a) && is_valid(&b));
        prop_assert_eq!(a.cmp(&b), numeric(&a, &b));
    }

    /// Any increasing pair admits a valid key strictly between it, in both
    /// orders, one digit past the longer bound at most. Any other pair is
    /// refused, including two spellings of one number.
    #[test]
    fn between_lands_strictly_inside_any_increasing_bounds((lo, hi) in bounds()) {
        let got = between(Some(&lo), Some(&hi));
        if numeric(&lo, &hi) != Ordering::Less {
            prop_assert_eq!(got, Err(SortOrderError::NotIncreasing));
            return Ok(());
        }
        let k = got.expect("increasing bounds admit a key");
        prop_assert!(is_valid(&k), "{:?}", k);
        prop_assert!(lo < k && k < hi, "{:?} < {:?} < {:?}", lo, k, hi);
        prop_assert_eq!(numeric(&lo, &k), Ordering::Less);
        prop_assert_eq!(numeric(&k, &hi), Ordering::Less);
        prop_assert!(k.len() <= lo.len().max(hi.len()) + 1, "{:?}", k);
    }

    /// A missing bound opens that end of the list; an empty lower bound is
    /// the same as a missing one.
    #[test]
    fn a_missing_bound_opens_that_end(k in digits(8)) {
        let above = between(Some(&k), None).expect("the end is always open");
        prop_assert!(is_valid(&above) && k < above, "{:?} < {:?}", k, above);
        prop_assert!(above.len() <= k.len() + 1);
        prop_assert_eq!(&above, &append_after(Some(&k)));

        let below = between(None, Some(&k));
        prop_assert_eq!(&below, &between(Some(""), Some(&k)));
        if k.bytes().all(|b| b == b'A') {
            prop_assert_eq!(below, Err(SortOrderError::NotIncreasing));
        } else {
            let below = below.expect("a non-zero upper bound has room below it");
            prop_assert!(is_valid(&below) && below < k, "{:?} < {:?}", below, k);
        }
    }

    /// `spread` gives `n` increasing keys inside the bounds, and stays short
    /// because it bisects. It refuses what `between` refuses, for every `n`.
    #[test]
    fn spread_fills_any_gap_in_order(
        (lo, hi) in bounds(),
        open_lo in any::<bool>(),
        open_hi in any::<bool>(),
        n in 0usize..=100,
    ) {
        let after = (!open_lo).then_some(lo.as_str());
        let before = (!open_hi).then_some(hi.as_str());
        let got = spread(after, before, n);
        let floor = after.unwrap_or("");
        if before.is_some_and(|hi| numeric(floor, hi) != Ordering::Less) {
            prop_assert_eq!(got, Err(SortOrderError::NotIncreasing));
            return Ok(());
        }
        let keys = got.expect("increasing bounds admit keys");
        prop_assert_eq!(keys.len(), n);
        prop_assert!(keys.windows(2).all(|w| w[0] < w[1]), "{:?}", keys);
        for k in &keys {
            prop_assert!(is_valid(k), "{:?}", k);
            prop_assert!(floor < k.as_str(), "{:?} < {:?}", floor, k);
            prop_assert!(before.is_none_or(|hi| k.as_str() < hi), "{:?} < {:?}", k, before);
        }
        let longest = keys.iter().map(String::len).max().unwrap_or(0);
        let bound = floor.len().max(before.map_or(0, str::len));
        prop_assert!(longest <= bound + 4, "{} keys grew to {}", n, longest);
        if n == 1 {
            prop_assert_eq!(&keys[0], &between(after, before).expect("as above"));
        }
    }
}
