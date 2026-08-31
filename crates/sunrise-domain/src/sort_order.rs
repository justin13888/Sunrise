//! Fractional indexing for `Stream.sort_order`, per `docs/02-domain/streams.md`
//! §Sort order.
//!
//! # What a key is
//!
//! A key is a string over the alphabet `A`..=`Z`, read as the digits of a
//! base-26 fraction: `"N"` is `0.N` ≈ ½, `"AN"` is `0.AN` ≈ ¹⁄₅₂. Because the
//! alphabet is a contiguous ASCII run and every key is compared against every
//! other from the left, **lexicographic order on the strings is numeric order
//! on the fractions**, so a database can sort a list with `ORDER BY sort_order`
//! and never decode anything. That equivalence is the entire point of the
//! encoding, and it is what [`between`] preserves.
//!
//! Moving one row therefore rewrites exactly one key. No sibling is touched,
//! which is what `docs/02-domain/streams.md` means by "reorders are constant
//! work; no list-shifting".
//!
//! # The trailing-`A` rule
//!
//! `A` is the digit zero, so `"AB"` and `"ABA"` and `"ABAA"` are all the same
//! *number* written three ways. Two rows holding two spellings of one number
//! would be un-orderable by the string comparison above — the whole scheme's
//! foundation — so **a key this module generates never ends in `A`**, which
//! makes the spelling of each number unique. [`is_valid`] enforces it, and
//! [`crate::StreamPatch::validate`] refuses a key that breaks it, so no such
//! key can be written by this build.
//!
//! [`between`] tolerates a trailing `A` on its *inputs*, because a key can
//! arrive from a peer running a build this one has never seen. It only ever
//! needs its bounds to be distinct numbers, and it produces a well-formed key
//! from any well-formed pair.
//!
//! # No jitter
//!
//! Real-world fractional-index libraries append random digits so that two
//! devices inserting at the same position get different keys and both survive.
//! This one is **deterministic**: under ADR-0014 a Stream is one entity-level
//! last-writer-wins unit, so two concurrent reorders do not both survive
//! whatever their keys are — one whole Stream row wins and the other's reorder
//! is discarded. Jitter would buy nothing, would make the same reorder produce
//! different bytes on different devices, and would need an injected RNG to
//! stay testable. See [`crate::stream::Stream::sort_order`].
//!
//! # Not implemented: defrag
//!
//! Repeatedly inserting at the same position lengthens the key by roughly one
//! digit per insertion, and nothing in v1 shortens it again:
//! `docs/02-domain/streams.md` specifies a `stream.list.defrag` op that
//! rewrites a whole list once a key reaches [`DEFRAG_THRESHOLD_BYTES`], and
//! that op does not exist in the op-kind registry
//! (`sunrise_core::inner_op`). The constant is here so the bound has one
//! definition when the op lands; **nothing reads it today**, and the index
//! really does grow unbounded under pathological reordering.

use crate::validation::ValidationError;

/// Number of digits in the key alphabet, `A`..=`Z`.
const BASE: u8 = 26;

/// The digit zero.
const ZERO: u8 = b'A';

/// The largest digit, `BASE - 1`.
const LAST: u8 = BASE - 1;

/// Key length at which `docs/02-domain/streams.md` says a device should
/// trigger `stream.list.defrag`.
///
/// **Unobserved in v1.** There is no defrag op to trigger, so this documents
/// the bound rather than enforcing it. See the module docs.
pub const DEFRAG_THRESHOLD_BYTES: usize = 64;

/// The dotted field path reported when a `sort_order` fails validation.
pub const FIELD: &str = "stream.sort_order";

/// Whether `key` is a key this build is willing to write.
///
/// Non-empty, `A`..=`Z` only, and not ending in the zero digit `A` — see the
/// module docs for why the last rule exists.
///
/// The empty string is **not** valid, and that is deliberate: it is the column
/// default for a Stream that has never been ordered, and it means "no key",
/// not "the first key". [`between`] accepts it as a lower bound (where it is
/// indistinguishable from `None`) and rejects it as an upper bound (where
/// nothing can sort before it).
#[must_use]
pub fn is_valid(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| (ZERO..ZERO + BASE).contains(&b))
        && !key.ends_with(ZERO as char)
}

/// Validate a `sort_order` on its way into a patch.
///
/// Reported as [`ValidationError::Field`] rather than a new variant: the wire
/// error code for a field-shape violation already exists and this is one.
pub fn validate(key: &str) -> Result<(), ValidationError> {
    if is_valid(key) {
        Ok(())
    } else {
        Err(ValidationError::Field {
            field: FIELD,
            constraint: "sort_order_key",
        })
    }
}

/// Why a pair of bounds admits no key between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SortOrderError {
    /// A bound was not a string of `A`..=`Z` digits.
    #[error("sort_order bound is not a base-26 key")]
    MalformedBound,
    /// The bounds were equal, or the lower one was the larger. There is no
    /// value strictly between them, and inventing one would reorder somebody's
    /// list at random.
    #[error("sort_order bounds are not strictly increasing")]
    NotIncreasing,
}

impl From<SortOrderError> for ValidationError {
    fn from(_: SortOrderError) -> Self {
        Self::Field {
            field: FIELD,
            constraint: "sort_order_bounds",
        }
    }
}

/// The digit at `i`, or the implicit zero past the end of a key.
///
/// Padding with zero is what makes `"AB"` and `"ABAAA"` the same number, and
/// it is why a shorter lower bound needs no special case anywhere below.
fn digit(key: &str, i: usize) -> u8 {
    key.as_bytes().get(i).map_or(0, |b| b - ZERO)
}

/// Whether every byte of `key` is a digit of the alphabet. The empty string
/// passes: it is the number zero, spelled with no digits.
fn well_formed(key: &str) -> bool {
    key.bytes().all(|b| (ZERO..ZERO + BASE).contains(&b))
}

/// A key that sorts strictly between `after` and `before`.
///
/// `None` for `after` means the start of the list; `None` for `before` means
/// the end of it. The empty string is accepted for `after` and means the same
/// as `None`.
///
/// The result is the shortest key the walk below reaches, which is what keeps
/// keys short under ordinary use: appending to a list of any length costs one
/// digit, and only *repeated* insertion at one spot grows them.
///
/// # Errors
///
/// [`SortOrderError::MalformedBound`] if either bound is not made of `A`..=`Z`
/// digits, and [`SortOrderError::NotIncreasing`] if `before` does not sort
/// strictly after `after` — including the case where the two spell the same
/// number differently, such as `"AB"` and `"ABA"`.
pub fn between(after: Option<&str>, before: Option<&str>) -> Result<String, SortOrderError> {
    let lo = after.unwrap_or("");
    if !well_formed(lo) {
        return Err(SortOrderError::MalformedBound);
    }
    let hi = match before {
        None => return Ok(after_key(lo)),
        Some(hi) if well_formed(hi) => hi,
        Some(_) => return Err(SortOrderError::MalformedBound),
    };

    // Strictly increasing as *numbers*, which trailing zeros can hide:
    // "AB" < "ABA" as strings but they are one number, and the walk below
    // would never terminate on them.
    let width = lo.len().max(hi.len());
    if (0..width).all(|i| digit(lo, i) == digit(hi, i)) || lo > hi {
        return Err(SortOrderError::NotIncreasing);
    }

    let mut out = Vec::with_capacity(width + 1);
    let mut i = 0;
    // Copy the digits the two bounds agree on. `hi` is never exhausted first:
    // that would mean `lo` starts with the whole of `hi`, so `lo >= hi`, which
    // the check above already refused.
    loop {
        let (a, b) = (digit(lo, i), digit(hi, i));
        if a != b {
            if b - a >= 2 {
                // Room to land between them in this digit alone.
                out.push(ZERO + a + (b - a) / 2);
                return Ok(finish(out));
            }
            // The digits are adjacent, so taking `a` here is the only way
            // down. That drops the upper bound entirely — every continuation
            // of `a` sorts below `hi` — and leaves only "beat the rest of
            // `lo`", which `above_tail` does.
            out.push(ZERO + a);
            above_tail(&mut out, lo, i + 1);
            return Ok(finish(out));
        }
        out.push(ZERO + a);
        i += 1;
    }
}

/// Extend `out` with the shortest suffix that sorts strictly above `lo`'s
/// digits from `i` on, with no upper bound to respect.
///
/// Runs off the end of `lo` in the worst case, where the padded digit is zero
/// and the midpoint below is `N` — so this always terminates.
fn above_tail(out: &mut Vec<u8>, lo: &str, mut i: usize) {
    loop {
        let a = digit(lo, i);
        if a < LAST {
            // Halfway between this digit and the top of the alphabet. Never
            // zero, because it is strictly greater than `a >= 0`.
            out.push(ZERO + a + (BASE - a) / 2);
            return;
        }
        out.push(ZERO + a);
        i += 1;
    }
}

/// A key that sorts strictly after `lo` (empty for "after nothing").
///
/// The append case, and the only one that cannot fail: there is always room
/// above any key. `after_key("")` is the midpoint of the whole space, `"N"`,
/// which is what the *first* stream in a vault gets.
fn after_key(lo: &str) -> String {
    let mut out = Vec::with_capacity(lo.len() + 1);
    above_tail(&mut out, lo, 0);
    finish(out)
}

/// Turn accumulated digits into a key.
///
/// Every path above ends by pushing a digit that is strictly greater than
/// zero, so the trailing-`A` rule holds by construction rather than by
/// trimming; the debug assertion is here to keep it that way.
fn finish(out: Vec<u8>) -> String {
    let key = String::from_utf8(out).unwrap_or_default();
    debug_assert!(is_valid(&key), "generated a malformed sort key: {key:?}");
    key
}

/// The key for a new entry appended to a list whose last key is `last`.
///
/// The default `docs/02-domain/streams.md` specifies for a new Stream:
/// "between the last and 'end'". Infallible, unlike [`between`], because the
/// end of a list is always open — see [`after_key`].
///
/// A malformed `last` cannot be improved on: no key over `A`..=`Z` sorts after
/// one containing a byte outside it. Callers pass the largest **well-formed**
/// key in the list, which is what `sunrise_core`'s query does.
#[must_use]
pub fn append_after(last: Option<&str>) -> String {
    let lo = last.filter(|k| well_formed(k)).unwrap_or("");
    after_key(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every generated key must be one this build would also accept back.
    #[track_caller]
    fn between_ok(a: Option<&str>, b: Option<&str>) -> String {
        let k = between(a, b).expect("bounds admit a key");
        assert!(is_valid(&k), "generated an invalid key {k:?}");
        if let Some(a) = a.filter(|s| !s.is_empty()) {
            assert!(k.as_str() > a, "{k:?} must sort after {a:?}");
        }
        if let Some(b) = b {
            assert!(k.as_str() < b, "{k:?} must sort before {b:?}");
        }
        k
    }

    #[test]
    fn the_first_key_in_an_empty_vault_is_the_midpoint() {
        assert_eq!(between(None, None).unwrap(), "N");
        assert_eq!(append_after(None), "N");
        assert_eq!(append_after(Some("")), "N");
    }

    #[test]
    fn appending_walks_up_the_alphabet_without_lengthening() {
        // Each append takes half the remaining room, so a list built by
        // appending stays one digit long for a good while.
        let mut k = append_after(None);
        assert_eq!(k, "N");
        for expected in ["T", "W", "Y", "Z"] {
            k = append_after(Some(&k));
            assert_eq!(k, expected);
        }
        // Only once the top digit is exhausted does a second one appear.
        assert_eq!(append_after(Some("Z")), "ZN");
        assert_eq!(append_after(Some("ZZ")), "ZZN");
        assert_eq!(append_after(Some("ZZZZ")), "ZZZZN");
    }

    #[test]
    fn inserting_at_the_start_shrinks_toward_zero() {
        assert_eq!(between_ok(None, Some("N")), "G");
        assert_eq!(between_ok(None, Some("G")), "D");
        assert_eq!(between_ok(None, Some("B")), "AN");
        assert_eq!(between_ok(None, Some("AB")), "AAN");
        // An empty lower bound is the start of the list, exactly like `None`.
        assert_eq!(between_ok(Some(""), Some("N")), "G");
    }

    #[test]
    fn a_wide_gap_is_split_in_one_digit() {
        assert_eq!(between_ok(Some("A"), Some("Z")), "M");
        assert_eq!(between_ok(Some("B"), Some("D")), "C");
    }

    /// The awkward case: neighbours with nothing between them at this length.
    #[test]
    fn adjacent_keys_lengthen_rather_than_collide() {
        // "B" and "C" are consecutive digits; the answer has to be longer.
        let k = between_ok(Some("B"), Some("C"));
        assert_eq!(k, "BN");
        // Same at depth, and with the lower bound already long.
        assert_eq!(between_ok(Some("AB"), Some("AC")), "ABN");
        assert_eq!(between_ok(Some("BZ"), Some("C")), "BZN");
        assert_eq!(between_ok(Some("BZZ"), Some("C")), "BZZN");
    }

    /// The pathological case the defrag op exists for and does not yet handle:
    /// dropping a row into the same gap over and over.
    #[test]
    fn repeated_insertion_at_one_spot_grows_the_key_without_bound() {
        // Always immediately below the previous winner. Each key is the
        // midpoint of what is left, so the gap shrinks by a factor of 26 per
        // digit and a new digit appears once the current one is exhausted.
        let mut hi = "C".to_string();
        let mut lengths = Vec::new();
        for _ in 0..400 {
            hi = between_ok(Some("B"), Some(&hi));
            lengths.push(hi.len());
        }
        assert!(
            lengths.windows(2).all(|w| w[0] <= w[1]),
            "keys must never get shorter on their own — that is what defrag is for"
        );
        // Past the bound `docs/02-domain/streams.md` says a device should
        // defrag at, with nothing to observe it. This is the assertion that
        // makes "grows unbounded under pathological reordering" a fact about
        // the code rather than a line in a doc.
        assert!(
            hi.len() > DEFRAG_THRESHOLD_BYTES,
            "expected to blow past {DEFRAG_THRESHOLD_BYTES} bytes, got {}",
            hi.len()
        );
        // Still a key, still in the right place, however long it got.
        assert!(is_valid(&hi));
        assert!(hi.as_str() > "B" && hi.as_str() < "C");
    }

    /// Insertions at the *bottom* of a shrinking gap, the mirror of the above,
    /// and the same growth for the same reason.
    #[test]
    fn repeated_insertion_just_after_a_key_also_grows() {
        let mut lo = "B".to_string();
        for _ in 0..400 {
            let prev = lo.clone();
            lo = between_ok(Some(&prev), Some("C"));
        }
        assert!(lo.len() > DEFRAG_THRESHOLD_BYTES);
        assert!(lo.as_str() > "B" && lo.as_str() < "C");
        assert!(is_valid(&lo));
    }

    #[test]
    fn boundaries_at_a_and_z_are_reachable() {
        // Below the smallest key there is always room: the answer grows a
        // leading run of the zero digit rather than running out.
        let mut k = "B".to_string();
        for _ in 0..200 {
            k = between_ok(None, Some(&k));
        }
        assert!(k.starts_with("AAA"), "{k:?} should have sunk toward zero");
        assert!(k.as_str() < "B" && is_valid(&k));

        // Above the largest, the mirror image: appending never runs out
        // either, it just grows a run of the top digit.
        let mut top = "Z".to_string();
        for _ in 0..200 {
            let prev = top.clone();
            top = append_after(Some(&prev));
            assert!(top > prev && is_valid(&top));
        }
        assert!(
            top.starts_with("ZZZ"),
            "{top:?} should have climbed to the top"
        );

        // And the two extremes still admit a key between them.
        between_ok(Some(&k), Some(&top));
    }

    #[test]
    fn a_generated_key_never_ends_in_the_zero_digit() {
        // Sweep every adjacent single-digit pair, plus both open ends.
        for a in b'A'..=b'Z' {
            let a = (a as char).to_string();
            assert!(!append_after(Some(&a)).ends_with('A'));
            if a != "A" {
                assert!(!between_ok(None, Some(&a)).ends_with('A'));
            }
            for b in (a.as_bytes()[0] + 1)..=b'Z' {
                let b = (b as char).to_string();
                assert!(!between_ok(Some(&a), Some(&b)).ends_with('A'));
            }
        }
    }

    #[test]
    fn equal_bounds_are_refused_rather_than_guessed_at() {
        assert_eq!(
            between(Some("N"), Some("N")),
            Err(SortOrderError::NotIncreasing)
        );
        assert_eq!(
            between(Some("C"), Some("B")),
            Err(SortOrderError::NotIncreasing)
        );
        assert_eq!(
            between(Some("ZZ"), Some("B")),
            Err(SortOrderError::NotIncreasing)
        );
        // Nothing sorts before the start of the list.
        assert_eq!(between(None, Some("")), Err(SortOrderError::NotIncreasing));
    }

    /// Two spellings of one number are equal, not adjacent. Refusing them is
    /// what stops the digit walk from running forever.
    #[test]
    fn bounds_that_spell_the_same_number_are_refused() {
        assert_eq!(
            between(Some("AB"), Some("ABA")),
            Err(SortOrderError::NotIncreasing)
        );
        assert_eq!(
            between(Some("AB"), Some("ABAAA")),
            Err(SortOrderError::NotIncreasing)
        );
        assert_eq!(
            between(Some(""), Some("A")),
            Err(SortOrderError::NotIncreasing)
        );
        assert_eq!(
            between(Some("A"), Some("AAA")),
            Err(SortOrderError::NotIncreasing)
        );
    }

    /// A key from a peer this build does not know is a bound like any other,
    /// as long as it is made of digits. A trailing `A` is tolerated on input.
    #[test]
    fn trailing_zero_bounds_from_elsewhere_still_work() {
        assert_eq!(between_ok(Some("ABA"), Some("ABB")), "ABAN");
        assert_eq!(append_after(Some("ABA")), "N");
        assert_eq!(between_ok(Some("A"), Some("AB")), "AAN");
    }

    #[test]
    fn bounds_outside_the_alphabet_are_refused() {
        // "a0" is the literal every build before this one hardcoded. It is not
        // a key: `a` sorts above `Z`, so nothing this module can generate
        // would sort after it.
        assert_eq!(
            between(Some("a0"), None),
            Err(SortOrderError::MalformedBound)
        );
        assert_eq!(
            between(Some("A"), Some("a0")),
            Err(SortOrderError::MalformedBound)
        );
        assert_eq!(
            between(Some("A-B"), Some("C")),
            Err(SortOrderError::MalformedBound)
        );
        // `append_after` cannot fail, so it ignores a bound it cannot use and
        // returns the midpoint rather than a key that would sort below it.
        assert_eq!(append_after(Some("a0")), "N");
    }

    #[test]
    fn validation_matches_what_between_produces() {
        assert!(is_valid("N") && is_valid("AAN") && is_valid("ZZZZN"));
        assert!(!is_valid(""), "the unset sentinel is not a key");
        assert!(!is_valid("NA"), "a trailing zero is a second spelling");
        assert!(!is_valid("a0") && !is_valid("N1") && !is_valid("n"));
        assert!(validate("N").is_ok());
        assert_eq!(
            validate("NA"),
            Err(ValidationError::Field {
                field: FIELD,
                constraint: "sort_order_key",
            })
        );
    }

    /// The property that everything else rests on: for any two keys, the
    /// string comparison and the numeric comparison agree, so `ORDER BY
    /// sort_order` in SQL is the same ordering this module reasons about.
    #[test]
    fn a_long_random_walk_stays_sorted() {
        // Deterministic pseudo-random: a linear congruential step, so this is
        // a fixed sequence and not a flake waiting to happen.
        let mut list = vec![append_after(None)];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..300 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let at = (state >> 33) as usize % (list.len() + 1);
            let lo = at.checked_sub(1).map(|i| list[i].as_str());
            let hi = list.get(at).map(String::as_str);
            let key = between(lo, hi).expect("neighbours always admit a key");
            list.insert(at, key);
        }
        assert_eq!(list.len(), 301);
        assert!(
            list.windows(2).all(|w| w[0] < w[1]),
            "the list stopped being sorted"
        );
        assert!(list.iter().all(|k| is_valid(k)));
    }
}
