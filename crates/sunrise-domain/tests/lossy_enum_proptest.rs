//! Every string-valued wire/storage enum is lossless (ADR-0045 §6).
//!
//! One property per enum: ANY string decodes, reads back through the storage
//! spelling (`as_str` / `from_raw`, the pair the storage projection writes and
//! reads), and re-encodes to the very bytes it arrived as. Before this, an
//! unfamiliar value decoded to a safe fallback and was then *written back as
//! that fallback*, so one write from an older build overwrote a newer peer's
//! value on every replica.
//!
//! The strategy is deliberately biased toward the known spellings and their
//! near misses (case, whitespace), because the interesting failures are a
//! known spelling that stops being recognised and a near miss that is
//! swallowed into a known arm.

use proptest::prelude::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;
use sunrise_cbor::{decode_canonical, encode_canonical};
use sunrise_domain::{
    ConstraintSeverity, Energy, FocusKind, Frequency, InterruptionReason, RRule,
    RoutineCatchupPolicy, StreamColor, StreamReviewCadence, TaskState, Weekday,
};

/// Arbitrary strings, weighted toward `known` spellings and near misses.
fn raw(known: &'static [&'static str]) -> impl Strategy<Value = String> {
    let pick = proptest::sample::select(known);
    prop_oneof![
        3 => pick.clone().prop_map(str::to_owned),
        2 => pick.clone().prop_map(str::to_uppercase),
        1 => pick.prop_map(|s| format!(" {s}")),
        4 => any::<String>(),
    ]
}

/// The whole round trip for one enum and one raw string.
fn round_trips<T>(s: &str, from_raw: fn(&str) -> T, as_str: fn(&T) -> &str, known: &[&str])
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    // Decode from the wire, exactly as it arrived.
    let wire = encode_canonical(&s).expect("a string always encodes");
    let decoded: T = decode_canonical(&wire).expect("an unknown value never fails decode");
    assert_eq!(decoded, from_raw(s), "wire decode and storage read agree");

    // Storage: the column holds `as_str`, and reading it back is `from_raw`.
    let column = as_str(&decoded).to_owned();
    assert_eq!(column, s, "the stored spelling is the raw string");
    let reread = from_raw(&column);
    assert_eq!(reread, decoded, "a database round trip does not degrade it");

    // Re-encode: byte for byte what arrived.
    let reencoded = encode_canonical(&reread).expect("re-encode");
    assert_eq!(reencoded, wire, "re-encodes byte for byte");

    // A known spelling is its known arm, never `Unknown`.
    let is_known = known.contains(&s);
    let debug = format!("{reread:?}");
    assert_eq!(
        !debug.starts_with("Unknown("),
        is_known,
        "{s:?} decoded to {debug}"
    );
}

macro_rules! lossless {
    ($name:ident, $ty:ident, [$($s:literal),+ $(,)?]) => {
        proptest! {
            #[test]
            fn $name(s in raw(&[$($s),+])) {
                round_trips::<$ty>(&s, $ty::from_raw, $ty::as_str, &[$($s),+]);
            }
        }
    };
}

lossless!(
    task_state,
    TaskState,
    ["todo", "in_progress", "done", "cancelled"]
);
lossless!(energy, Energy, ["low", "med", "high"]);
lossless!(constraint_severity, ConstraintSeverity, ["hard", "soft"]);
lossless!(
    stream_review_cadence,
    StreamReviewCadence,
    ["weekly", "biweekly", "monthly", "none"]
);
lossless!(
    routine_catchup_policy,
    RoutineCatchupPolicy,
    ["skip", "merge", "queue"]
);
lossless!(focus_kind, FocusKind, ["work", "break"]);
lossless!(
    interruption_reason,
    InterruptionReason,
    ["self", "meeting", "blocked", "other"]
);
lossless!(
    stream_color,
    StreamColor,
    ["slate", "rose", "amber", "emerald", "sky", "indigo", "violet", "pink"]
);
lossless!(
    frequency,
    Frequency,
    ["DAILY", "WEEKLY", "MONTHLY", "YEARLY"]
);
lossless!(weekday, Weekday, ["SU", "MO", "TU", "WE", "TH", "FR", "SA"]);

proptest! {
    /// A whole rule carrying unknown values — in `FREQ`, `BYDAY` and `WKST`
    /// at once — survives its canonical CBOR, the form the op carries and
    /// `routines.rrule_cbor` stores, byte for byte; and a rule holding any of them
    /// is flagged rather than expanded.
    #[test]
    fn a_rule_with_unknown_values_round_trips_and_is_flagged(
        freq in raw(&["DAILY", "WEEKLY", "MONTHLY", "YEARLY"]),
        days in proptest::collection::vec(raw(&["MO", "TU", "SU"]), 0..4),
        wkst in proptest::option::of(raw(&["MO", "SU"])),
    ) {
        let rule = RRule {
            freq: Frequency::from_raw(&freq),
            interval: 1,
            by_day: days.iter().map(|d| Weekday::from_raw(d)).collect(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            by_set_pos: Vec::new(),
            count: None,
            until: None,
            wkst: wkst.as_deref().map(Weekday::from_raw),
        };
        let bytes = encode_canonical(&rule).unwrap();
        let back: RRule = decode_canonical(&bytes).unwrap();
        prop_assert_eq!(&back, &rule);
        prop_assert_eq!(encode_canonical(&back).unwrap(), bytes);

        let any_unknown = rule.freq.is_unknown()
            || rule.by_day.iter().any(Weekday::is_unknown)
            || rule.wkst.as_ref().is_some_and(Weekday::is_unknown);
        prop_assert_eq!(rule.is_understood(), !any_unknown);
    }
}

/// Logic reads an unknown value as the named safe fallback; encoding never
/// writes that fallback.
#[test]
fn unknown_values_read_as_their_safe_fallback() {
    assert_eq!(TaskState::from_raw("archived").effective(), TaskState::Todo);
    assert_eq!(Energy::from_raw("frantic").effective(), Energy::Med);
    assert_eq!(
        ConstraintSeverity::from_raw("critical").effective(),
        ConstraintSeverity::Soft
    );
    assert_eq!(
        StreamReviewCadence::from_raw("quarterly").effective(),
        StreamReviewCadence::Weekly
    );
    assert_eq!(
        RoutineCatchupPolicy::from_raw("spread").effective(),
        RoutineCatchupPolicy::Skip
    );
    assert_eq!(FocusKind::from_raw("nap").effective(), FocusKind::Work);
    assert_eq!(
        InterruptionReason::from_raw("phone").effective(),
        InterruptionReason::Other
    );
    assert_eq!(
        StreamColor::from_raw("teal").effective(),
        StreamColor::Slate
    );
    // …and the value itself still says what it is.
    assert_eq!(TaskState::from_raw("archived").as_str(), "archived");
}

/// An unknown value survives inside the value that carries it, and logic reads
/// it as the fallback: a constraint of unknown severity round-trips byte for
/// byte, and never hard-blocks.
#[test]
fn an_unknown_severity_survives_its_constraint_and_reads_as_soft() {
    use sunrise_domain::constraint::{violations_by_severity, ScheduleConstraint, WeekdaySet};
    use sunrise_domain::Weekday;

    let c = ScheduleConstraint {
        time_of_day: None,
        days_of_week: WeekdaySet::from_days([Weekday::Mo]),
        date_range: None,
        severity: ConstraintSeverity::from_raw("critical"),
    };
    let bytes = encode_canonical(&c).unwrap();
    let back: ScheduleConstraint = decode_canonical(&bytes).unwrap();
    assert_eq!(back, c);
    assert_eq!(encode_canonical(&back).unwrap(), bytes);

    // A Saturday violates it; as `soft`, not `hard`.
    let saturday = jiff::civil::date(2026, 8, 8)
        .at(12, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap();
    let (hard, soft) = violations_by_severity(std::slice::from_ref(&c), &saturday);
    assert!(hard.is_empty(), "an unknown severity never hard-blocks");
    assert_eq!(soft, vec![c]);
}
