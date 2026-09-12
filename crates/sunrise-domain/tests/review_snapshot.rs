//! `ReviewSnapshot`'s window accessors.
//!
//! A snapshot is a **persisted, op-replicated** entity: it is written once,
//! never updated, and read back by the History view on every device. Its two
//! window accessors are the only way anything outside this crate learns what
//! period a saved review covered — and until this file, the only assertion on
//! either in the whole workspace was inside a two-core end-to-end test in
//! another crate, where a wrong answer reads as a sync failure rather than as
//! an accessor returning the wrong field.
//!
//! Deliberately **not** in `tests/serde_compat.rs`: that file's charter is the
//! frozen pre-jiff chrono-era fixtures, and its naming is built around them.
//! This is a live-type test, not a compatibility one.

#![allow(clippy::unwrap_used)]

use jiff::Timestamp;
use sunrise_cbor::{decode_canonical, encode_canonical};
use sunrise_domain::review::{ReviewSnapshot, ReviewSnapshotStream, ReviewTotals, StreakRow};
use sunrise_domain::Unknowns;
use sunrise_id::{EntityKind, EntityRef};

/// 2026-01-05T00:00:00Z is a **Monday**, so the window below is one whole
/// civil week and its boundaries are checkable by hand.
const MON: i64 = 1_767_571_200_000;
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn eref(kind: EntityKind, b: u8) -> EntityRef {
    EntityRef::new(kind, [b; 16])
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap()
}

/// Every field spelled out, so a field added to the entity breaks this rather
/// than defaulting quietly into a snapshot nobody asserted.
fn snapshot(window_start: Timestamp, window_end: Timestamp) -> ReviewSnapshot {
    ReviewSnapshot {
        id: eref(EntityKind::ReviewSnapshot, 1),
        created_at: ts(MON + WEEK_MS),
        window_start,
        window_end,
        totals: ReviewTotals {
            completed: 8,
            deferred: 3,
            dropped: 1,
            created: 11,
            reopened: 2,
        },
        streams: vec![ReviewSnapshotStream {
            stream: eref(EntityKind::Stream, 3),
            name: "Work".into(),
            completed: 5,
            deferred: 2,
            created: 6,
        }],
        streaks: vec![StreakRow {
            routine: eref(EntityKind::Routine, 4),
            title: "Stretch".into(),
            streak: 12,
            last_completed_at_ms: Some(u64::try_from(MON + 3 * 86_400_000).unwrap()),
        }],
        note: Some("felt scattered".into()),
        unknown: Unknowns::new(),
    }
}

#[test]
fn the_window_accessors_report_the_window_the_review_covered() {
    let s = snapshot(ts(MON), ts(MON + WEEK_MS));
    assert_eq!(
        s.window_start_ms(),
        u64::try_from(MON).unwrap(),
        "the accessor must report the start, not the creation instant"
    );
    assert_eq!(
        s.window_end_ms(),
        u64::try_from(MON + WEEK_MS).unwrap(),
        "and the end, which is exclusive — a week later, not six days"
    );
    assert_eq!(
        s.window_end_ms() - s.window_start_ms(),
        u64::try_from(WEEK_MS).unwrap()
    );
    // The two accessors read two different fields. Asserting one of them
    // against a symmetric fixture would not notice them being swapped.
    assert_ne!(s.window_start_ms(), s.window_end_ms());
    assert_ne!(
        s.window_start_ms(),
        u64::try_from(s.created_at.as_millisecond()).unwrap()
    );
}

/// A pre-epoch window start cannot be expressed in the unsigned answer, and
/// clamps to zero rather than wrapping to something astronomical. Only
/// reachable from a corrupt or hostile row, which is exactly when a wrapped
/// `u64` would be worst.
#[test]
fn a_pre_epoch_window_clamps_to_zero_rather_than_wrapping() {
    // Both bounds pre-epoch, so each accessor is clamping its own field
    // rather than one of them happening to be zero already.
    let s = snapshot(ts(-2), ts(-1));
    assert_eq!(s.window_start_ms(), 0);
    assert_eq!(s.window_end_ms(), 0);
}

/// The Rust side holds `Timestamp`; the wire holds the integer milliseconds it
/// always held, under the keys it always used. The accessors and the wire form
/// must agree, or History on one device disagrees with History on another.
#[test]
fn the_wire_keeps_the_millisecond_fields_the_accessors_report() {
    let s = snapshot(ts(MON), ts(MON + WEEK_MS));
    let bytes = encode_canonical(&s).unwrap();
    let back: ReviewSnapshot = decode_canonical(&bytes).unwrap();
    assert_eq!(back, s, "a snapshot survives a round trip unchanged");
    assert_eq!(back.window_start_ms(), s.window_start_ms());
    assert_eq!(back.window_end_ms(), s.window_end_ms());

    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(
        json.get("window_start_ms")
            .and_then(serde_json::Value::as_i64),
        Some(MON),
        "the wire key is still `window_start_ms`, and still an integer"
    );
    assert_eq!(
        json.get("window_end_ms")
            .and_then(serde_json::Value::as_i64),
        Some(MON + WEEK_MS)
    );
}
