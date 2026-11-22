//! The streak across a shift: which policies excuse the days it covers.

use super::*;

/// "09:00 UTC" daily from August 1 2026, completed on the 1st to 5th and the
/// 11th to 15th, with a shift covering the 6th to the 10th.
fn streak_across_a_trip() -> (Routine, [(u8, Shift); 1]) {
    let mut r = daily("UTC", date(2026, 8, 1).at(9, 0, 0, 0), false);
    r.streak_keys = (1..=5)
        .chain(11..=15)
        .map(|d| format!("2026-08-{d:02}T09:00"))
        .collect();
    let trip = shift(
        "Trip",
        utc(2026, 8, 6, 0, 0),
        Some(utc(2026, 8, 11, 0, 0)),
        None,
    );
    (r, [(1, trip)])
}

/// Five missed days break the streak with no shift. A `skip` or `pause`
/// shift over them excuses them, spending no forgiveness; `shift_tz` and
/// `unchanged` do not.
#[test]
fn a_streak_holds_across_a_skip_window() {
    let (r, shifts) = streak_across_a_trip();
    assert_eq!(r.derived_streak().streak_counter, 5);

    let skip = r.derived_streak_across(&shifts, &ShiftPolicy::Skip);
    assert_eq!(skip.streak_counter, 10);
    assert_eq!(skip.forgivenesses_in_window, 0);
    assert_eq!(skip.streak_started_at, Some(utc(2026, 8, 1, 9, 0)));
    assert_eq!(r.derived_streak_across(&shifts, &ShiftPolicy::Pause), skip);

    for policy in [ShiftPolicy::ShiftTz, ShiftPolicy::Unchanged] {
        assert_eq!(
            r.derived_streak_across(&shifts, &policy),
            r.derived_streak()
        );
    }
    let none: [(u8, Shift); 0] = [];
    assert_eq!(
        r.derived_streak_across(&none, &ShiftPolicy::Skip),
        r.derived_streak()
    );
}

/// A miss the shift does not cover still spends the allowance as before.
#[test]
fn a_miss_outside_the_shift_still_counts() {
    let (mut r, shifts) = streak_across_a_trip();
    r.streak_keys.retain(|k| k != "2026-08-12T09:00");
    let s = r.derived_streak_across(&shifts, &ShiftPolicy::Skip);
    assert_eq!(s.streak_counter, 9);
    assert_eq!(s.forgivenesses_in_window, 1);
}

/// A shift still running at the read: the days it covers do not lapse the
/// streak under `skip`, and do under `unchanged`.
#[test]
fn a_running_skip_shift_keeps_the_streak_from_lapsing() {
    let mut r = daily("UTC", date(2026, 8, 1).at(9, 0, 0, 0), false);
    r.streak_keys = (1..=5).map(|d| format!("2026-08-{d:02}T09:00")).collect();
    r.derived_streak().apply_to(&mut r);
    assert_eq!(r.streak_counter, 5);
    let shifts = [(1u8, shift("Trip", utc(2026, 8, 6, 0, 0), None, None))];
    let now = utc(2026, 8, 10, 12, 0);
    let utc_tz = TimeZone::UTC;
    assert_eq!(r.streak_as_of(now, &utc_tz).streak_counter, 0);
    let held = r.streak_as_of_across(now, &utc_tz, &shifts, &ShiftPolicy::Skip);
    assert_eq!(held.streak_counter, 5);
    assert_eq!(held.forgivenesses_in_window, 0);
    assert_eq!(
        r.streak_as_of_across(now, &utc_tz, &shifts, &ShiftPolicy::Unchanged)
            .streak_counter,
        0
    );
}
