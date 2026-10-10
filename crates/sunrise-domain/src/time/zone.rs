//! Whether two reported zones are one zone to a reader.
//!
//! Kept apart from the value's semantics in the parent module because it
//! compares zones, not times: it changes when what counts as a zone change
//! does (`docs/10-cross-cutting/time.md` §7), and the parent when what a time
//! means does.

use jiff::tz::TimeZone;
use jiff::Timestamp;

/// How far either side of the moment they are compared at two zones must
/// agree for [`same_zone`] to call them one zone: 20 years, which covers
/// every open task a reader plausibly holds.
const SAME_ZONE_SPAN_SECS: i64 = 20 * 366 * 24 * 60 * 60;

/// Whether a reader in `a` and a reader in `b` resolve every civil time the
/// same way within 20 years of `around`: the same offset at
/// the start of that span and the same transitions through it.
///
/// Names are not the test. `Asia/Calcutta` is a tzdb link to `Asia/Kolkata`,
/// and `UTC`, `Etc/UTC` and `GMT` are one zone under three spellings, so a
/// reader whose zone is reported under another name has not moved
/// (`docs/10-cross-cutting/time.md` §7).
#[must_use]
pub fn same_zone(a: &TimeZone, b: &TimeZone, around: Timestamp) -> bool {
    if a == b {
        return true;
    }
    let span = jiff::SignedDuration::from_secs(SAME_ZONE_SPAN_SECS);
    let from = around.saturating_sub(span).unwrap_or(Timestamp::MIN);
    let until = around.saturating_add(span).unwrap_or(Timestamp::MAX);
    if a.to_offset(from) != b.to_offset(from) {
        return false;
    }
    let within = |t: &jiff::tz::TimeZoneTransition<'_>| t.timestamp() <= until;
    let mut ta = a.following(from).take_while(within);
    let mut tb = b.following(from).take_while(within);
    loop {
        match (ta.next(), tb.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.timestamp() == y.timestamp() && x.offset() == y.offset() => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An alias, or UTC under another spelling, is the same zone; a zone
    /// with the same offset today but other transitions is not.
    #[test]
    fn same_zone_compares_rules_not_names() {
        let at: Timestamp = "2026-03-04T12:00:00Z".parse().unwrap();
        let get = |n: &str| TimeZone::get(n).unwrap();
        assert!(same_zone(&get("Asia/Calcutta"), &get("Asia/Kolkata"), at));
        assert!(same_zone(&TimeZone::UTC, &get("Etc/UTC"), at));
        assert!(same_zone(&TimeZone::UTC, &get("GMT"), at));
        // Bogotá is UTC−5 all year, as New York is in March, but keeps no DST.
        let ny = get("America/New_York");
        assert!(!same_zone(&ny, &get("America/Bogota"), at));
        assert!(!same_zone(&TimeZone::UTC, &get("Asia/Kolkata"), at));
    }
}
