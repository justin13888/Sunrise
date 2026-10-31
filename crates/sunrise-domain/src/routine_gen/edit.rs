//! A routine's series across edits (`docs/02-domain/routines-and-recurrence.md`
//! §Edit scope, §Catch-up for occurrences already materialized; #331): how
//! far an edit reaches, how an edit "this and future" splits the series,
//! which routine of a split chain owns a key, and which past occurrences the
//! catch-up policy lapses at read time.
//!
//! Pure functions of routines and keys, with no clock and no storage, so
//! every replica derives the same split, the same owner and the same lapsed
//! set from the same data.

use super::{expand, occurrence_task_id, parse_occurrence_key, ExpandError};
use crate::routine::{Routine, RoutineCatchupPolicy, RoutinePatch};
use crate::validation::ValidationError;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::{EntityKind, EntityRef};
use thiserror::Error;

/// How far an edit of a routine reaches (§Edit scope).
///
/// Each occurrence is named by its key, the intended civil start
/// `YYYY-MM-DDTHH:MM` (§Occurrence key).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope")]
pub enum EditScope {
    /// One occurrence: the edit is written onto that occurrence's task only.
    This {
        /// The occurrence.
        key: String,
    },
    /// The occurrence at `key` and every one after it: the series is split
    /// there, the old routine ending before it and a new one continuing it.
    ThisAndFuture {
        /// The first occurrence the edit applies to.
        key: String,
    },
    /// The whole series: the routine itself is edited.
    #[default]
    All,
}

impl EditScope {
    /// The occurrence key the scope names, if any.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::This { key } | Self::ThisAndFuture { key } => Some(key),
            Self::All => None,
        }
    }
}

impl RoutinePatch {
    /// Whether the patch changes nothing but what one occurrence's task can
    /// carry: the template, the anchor's time and the constraint list. What
    /// an edit of scope [`crate::EditScope::This`] may hold.
    #[must_use]
    pub const fn is_occurrence_edit(&self) -> bool {
        self.rrule.is_none()
            && self.timezone.is_none()
            && self.ends_at.is_none()
            && self.catchup_policy.is_none()
            && self.grace_window_s.is_none()
            && self.forgiveness_enabled.is_none()
            && self.paused.is_none()
            && self.paused_until.is_none()
            && self.archived.is_none()
    }

    /// Apply the patch to `routine`, touching `updated_at` to `now`, and say
    /// what it moved. Validates the times the result holds.
    ///
    /// # Errors
    /// [`ValidationError::Field`] when the patched anchor or end is invalid
    /// ([`Routine::validate_times`]).
    pub fn apply_to(
        self,
        routine: &mut Routine,
        now: Timestamp,
    ) -> Result<PatchEffect, ValidationError> {
        let mut effect = PatchEffect::default();
        if let Some(t) = self.template {
            effect.template |= routine.template != t;
            routine.template = t;
        }
        if let Some(r) = self.rrule {
            effect.structural |= routine.rrule != r;
            routine.rrule = r;
        }
        // A zone change keeps every wall-clock time (ADR-0066 §1), so no key
        // changes and nothing is regenerated: the untouched occurrences
        // already planned in the old zone are re-pinned instead.
        if let Some(tz) = self.timezone {
            if routine.timezone != tz {
                effect.rezoned_from = Some(routine.timezone.clone());
                routine.rezone(tz);
            }
        }
        if let Some(s) = self.starts_at {
            effect.structural |= routine.starts_at != s;
            routine.starts_at = s;
        }
        if let Some(e) = self.ends_at {
            effect.structural |= routine.ends_at != e;
            routine.ends_at = e;
        }
        routine.normalize_legacy_times();
        routine.validate_times()?;
        if let Some(list) = self.scheduling_constraints {
            effect.template |= routine.scheduling_constraints != list;
            routine.scheduling_constraints = list;
        }
        if let Some(c) = self.catchup_policy {
            routine.catchup_policy = c;
        }
        if let Some(g) = self.grace_window_s {
            routine.grace_window_s = g;
        }
        if let Some(f) = self.forgiveness_enabled {
            routine.forgiveness_enabled = f;
        }
        if let Some(p) = self.paused {
            routine.paused = p;
        }
        if let Some(pu) = self.paused_until {
            routine.paused_until = pu;
        }
        if let Some(a) = self.archived {
            routine.archived = a;
        }
        routine.updated_at = now;
        Ok(effect)
    }
}

/// What [`RoutinePatch::apply_to`] moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchEffect {
    /// Which occurrences exist changed: the rule, the anchor or the end.
    pub structural: bool,
    /// What each occurrence is made of changed: the template or the
    /// constraint list, which propagate to untouched occurrences.
    pub template: bool,
    /// The zone the routine moved from, when it moved.
    pub rezoned_from: Option<String>,
}

/// Why a routine cannot be split at a key.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SplitError {
    /// The key is not `YYYY-MM-DDTHH:MM`.
    #[error("not an occurrence key: {0}")]
    BadKey(String),
    /// The routine's rule does not produce the key, or its anchor is of a
    /// kind this build cannot read.
    #[error("the series has no occurrence at {0}")]
    NotAnOccurrence(String),
    /// The rule could not be expanded up to the key.
    #[error(transparent)]
    Expand(#[from] ExpandError),
}

/// The two routines a split leaves (§Edit scope, *This and future*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesSplit {
    /// The routine split, ended before the key: its rule's `UNTIL` is the
    /// last occurrence before it, or it is archived when it has none.
    pub ended: Routine,
    /// The routine that continues the series from the key, before the edit
    /// is applied to it.
    pub continuation: Routine,
}

/// The id of the routine that continues series `root` from occurrence `key`.
///
/// A function of the pair, so two devices that split the same series at the
/// same occurrence create one routine and merge its fields.
#[must_use]
pub fn split_routine_id(root: &EntityRef, key: &str) -> EntityRef {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sunrise.routine_split.v1");
    hasher.update(root.bytes());
    hasher.update(key.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityRef::new(EntityKind::Routine, bytes)
}

/// A civil value as the UTC instant at the same wall clock: the frame the
/// split works in, which has no DST to move a key.
fn civil_utc(civil: DateTime) -> Option<Timestamp> {
    TimeZone::UTC.to_zoned(civil).ok().map(|z| z.timestamp())
}

impl Routine {
    /// The root of this routine's series: [`Routine::split_from`], or this
    /// routine itself when it was never split off. Every occurrence's task id
    /// is keyed on it ([`Routine::occurrence_task_id`]).
    #[must_use]
    pub fn series_root(&self) -> EntityRef {
        self.split_from.unwrap_or(self.id)
    }

    /// The task id of this series' occurrence `key`: keyed on the series
    /// root, so a routine that continues a split series names the
    /// occurrences its predecessor generated with the same ids, and adopts
    /// them rather than generating them twice.
    #[must_use]
    pub fn occurrence_task_id(&self, key: &str) -> EntityRef {
        occurrence_task_id(&self.series_root(), key)
    }

    /// The intended wall-clock start of occurrence `key`, when this
    /// routine's rule produces it (before its end, pause and skips): the
    /// key's minute with the anchor's seconds, which the key leaves out.
    #[must_use]
    pub fn occurrence_civil(&self, key: &str) -> Option<DateTime> {
        let r = self.clone().normalized();
        let anchor = r.anchor_civil()?;
        let at = civil_utc(parse_occurrence_key(key)?)?;
        let end = at
            .checked_add(jiff::SignedDuration::from_mins(1))
            .unwrap_or(at);
        expand(&r.rrule, anchor, &TimeZone::UTC, (at, end))
            .ok()?
            .into_iter()
            .find(|o| o.key == key)
            .map(|o| o.civil)
    }

    /// Whether this routine's rule produces occurrence `key`, before its end,
    /// pause and skips.
    #[must_use]
    pub fn produces_key(&self, key: &str) -> bool {
        self.occurrence_civil(key).is_some()
    }

    /// Whether occurrence `key` is one this routine has: its rule produces
    /// it ([`Routine::produces_key`]), it is not past the routine's end, and
    /// it is not skipped. Not its pause, which holds generation back without
    /// taking the occurrence out of the series.
    #[must_use]
    pub fn has_occurrence(&self, key: &str) -> bool {
        if !self.produces_key(key) || self.skipped_keys.iter().any(|k| k == key) {
            return false;
        }
        let r = self.clone().normalized();
        let (Some(end), Some(civil)) = (r.ends_at.as_ref(), r.occurrence_civil(key)) else {
            return true;
        };
        let tz = TimeZone::get(&r.timezone).unwrap_or(TimeZone::UTC);
        let Some(at) = r.occurrence_instant(civil, &tz) else {
            return false;
        };
        super::within_bound(end, civil, at, &r.occurrence_zone(&tz)).unwrap_or(false)
    }

    /// Split this series at occurrence `key` (§Edit scope, *This and
    /// future*), at `now`.
    ///
    /// The ended routine keeps every occurrence before `key`: its rule's
    /// `UNTIL` becomes the civil start of the last one, of the anchor's kind,
    /// or, when there is none, it is archived. The continuation is this
    /// routine from `key` on: id [`split_routine_id`] of the series root and
    /// `key`, `split_from` the root, anchored at `key` in the anchor's kind,
    /// a `COUNT` reduced by the occurrences the ended routine keeps, and the
    /// skips and streak keys at or after `key`. Its streak starts at the
    /// split: the streak fields a full-state op carries are reset, as the
    /// derivation from its keys would read them.
    ///
    /// # Errors
    /// [`SplitError`] when `key` is not one of this series' occurrences.
    pub fn split_at(&self, key: &str, now: Timestamp) -> Result<SeriesSplit, SplitError> {
        parse_occurrence_key(key).ok_or_else(|| SplitError::BadKey(key.into()))?;
        let civil = self
            .occurrence_civil(key)
            .ok_or_else(|| SplitError::NotAnOccurrence(key.into()))?;
        let r = self.clone().normalized();
        let anchor = r
            .anchor_civil()
            .ok_or_else(|| SplitError::NotAnOccurrence(key.into()))?;
        let (Some(from), Some(to)) = (civil_utc(anchor), civil_utc(civil)) else {
            return Err(SplitError::NotAnOccurrence(key.into()));
        };
        let before = expand(&r.rrule, anchor, &TimeZone::UTC, (from, to))?;

        let mut ended = r.clone();
        match before.last() {
            Some(last) => ended.rrule.until = Some(r.occurrence_time(last.civil)),
            None => ended.archived = true,
        }
        ended.updated_at = now;

        let root = r.series_root();
        let mut next = r.clone();
        next.id = split_routine_id(&root, key);
        next.split_from = Some(root);
        next.created_at = now;
        next.updated_at = now;
        next.starts_at = r.occurrence_time(civil);
        next.rrule.count = r
            .rrule
            .count
            .map(|c| c.saturating_sub(u32::try_from(before.len()).unwrap_or(u32::MAX)));
        next.skipped_keys.retain(|k| k.as_str() >= key);
        next.streak_keys.retain(|k| k.as_str() >= key);
        next.streak_counter = 0;
        next.last_completed_at = None;
        next.streak_started_at = None;
        next.forgivenesses_in_window = 0;
        next.archived = false;
        next.deleted = false;
        Ok(SeriesSplit {
            ended,
            continuation: next,
        })
    }

    /// Whether this routine owns occurrence `key` among `chain`, the live
    /// routines of its series (§Edit scope, *Which series owns a key*): the
    /// owner is the one with the latest anchor at or before the key, ties
    /// broken by the greater id. Only the owner generates the key, from its
    /// template, so a concurrent split at two keys leaves each key generated
    /// once.
    #[must_use]
    pub fn owns_key(&self, key: &str, chain: &[Self]) -> bool {
        if parse_occurrence_key(key).is_none() {
            return false;
        }
        // Anchors compare as keys, at the minute: a key carries no seconds,
        // so an anchor at 09:00:20 is at or before the key `…T09:00`.
        let anchor_of = |r: &Self| {
            r.clone()
                .normalized()
                .anchor_civil()
                .map(|a| super::format_key(a.date(), a.time()))
        };
        let Some(mine) = anchor_of(self) else {
            return false;
        };
        if mine.as_str() > key {
            return false;
        }
        let root = self.series_root();
        !chain.iter().any(|other| {
            other.id != self.id
                && !other.deleted
                && other.series_root() == root
                && anchor_of(other)
                    .is_some_and(|a| a.as_str() <= key && (&a, other.id) > (&mine, self.id))
        })
    }
}

/// What a routine's catch-up policy shows of its past open occurrences at
/// read time (§Catch-up for occurrences already materialized). Derived;
/// nothing is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchupView {
    /// The occurrences hidden from Today, lists and the triage queue, and
    /// listed under the routine's "Lapsed".
    pub lapsed: BTreeSet<EntityRef>,
    /// The occurrence a `merge` policy shows in place of the lapsed ones,
    /// with how many past open occurrences it stands for.
    pub merged: BTreeMap<EntityRef, usize>,
}

impl CatchupView {
    /// Fold another routine's view into this one.
    pub fn extend(&mut self, other: Self) {
        self.lapsed.extend(other.lapsed);
        self.merged.extend(other.merged);
    }
}

/// The catch-up view of one routine's past open occurrences, `past_open`
/// oldest first, under `policy`.
///
/// `skip` lapses every one; `merge` lapses all but the latest and shows the
/// latest for all of them, once there are two; `queue` shows every one. An
/// unknown policy lapses nothing: an occurrence a newer build chose to
/// generate is shown as an ordinary task, not hidden.
#[must_use]
pub fn catchup_view(policy: &RoutineCatchupPolicy, past_open: &[EntityRef]) -> CatchupView {
    let mut view = CatchupView::default();
    match policy {
        RoutineCatchupPolicy::Skip => view.lapsed.extend(past_open.iter().copied()),
        RoutineCatchupPolicy::Merge => {
            if let [older @ .., latest] = past_open {
                if !older.is_empty() {
                    view.lapsed.extend(older.iter().copied());
                    view.merged.insert(*latest, past_open.len());
                }
            }
        }
        RoutineCatchupPolicy::Queue | RoutineCatchupPolicy::Unknown(_) => {}
    }
    view
}

/// The title an occurrence standing for `n` missed ones shows: generation's
/// `merge` and the read-time view's share it.
#[must_use]
pub fn catchup_title(title: &str, n: usize) -> String {
    format!("{title} (x{n} catch-up)")
}

#[cfg(test)]
mod split_properties;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::TaskTemplate;
    use crate::rrule::RRule;
    use crate::time::SunriseTime;
    use crate::unknown::Unknowns;

    pub(super) fn routine(rule: &str, anchor: &str) -> Routine {
        let civil: DateTime = anchor.parse().expect("civil");
        Routine {
            id: EntityRef::new(EntityKind::Routine, [7; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            template: TaskTemplate {
                title: "Water".into(),
                stream_id: None,
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: Unknowns::new(),
            },
            rrule: RRule::parse(rule).expect("rule"),
            timezone: "America/New_York".into(),
            starts_at: SunriseTime::zoned(civil, "America/New_York"),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: vec!["2026-03-02T09:00".into(), "2026-03-09T09:00".into()],
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 3,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            split_from: None,
            streak_keys: vec!["2026-03-03T09:00".into(), "2026-03-08T09:00".into()],
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    pub(super) fn now() -> Timestamp {
        "2026-03-05T12:00:00Z".parse().expect("ts")
    }

    #[test]
    fn a_split_ends_the_old_series_before_the_key_and_continues_it_there() {
        let r = routine("FREQ=DAILY", "2026-03-01T09:00");
        let s = r.split_at("2026-03-06T09:00", now()).expect("splits");
        let until = "2026-03-05T09:00".parse().expect("civil");
        assert_eq!(
            s.ended.rrule.until,
            Some(SunriseTime::zoned(until, "America/New_York"))
        );
        assert!(!s.ended.archived);
        let n = &s.continuation;
        assert_eq!(n.id, split_routine_id(&r.id, "2026-03-06T09:00"));
        assert_eq!(n.split_from, Some(r.id));
        assert_eq!(n.series_root(), r.id);
        assert_eq!(n.anchor_civil(), Some("2026-03-06T09:00".parse().unwrap()));
        assert_eq!(n.skipped_keys, vec!["2026-03-09T09:00".to_owned()]);
        assert_eq!(n.streak_keys, vec!["2026-03-08T09:00".to_owned()]);
        assert_eq!(n.streak_counter, 0);
        // The ids of the occurrences after the key do not move.
        assert_eq!(
            n.occurrence_task_id("2026-03-07T09:00"),
            r.occurrence_task_id("2026-03-07T09:00")
        );
    }

    #[test]
    fn a_split_of_a_split_keeps_the_root() {
        let r = routine("FREQ=DAILY", "2026-03-01T09:00");
        let a = r.split_at("2026-03-06T09:00", now()).unwrap().continuation;
        let b = a.split_at("2026-03-08T09:00", now()).unwrap().continuation;
        assert_eq!(b.split_from, Some(r.id));
        assert_eq!(b.id, split_routine_id(&r.id, "2026-03-08T09:00"));
    }

    #[test]
    fn a_split_at_the_first_occurrence_archives_the_old_routine() {
        let r = routine("FREQ=DAILY", "2026-03-01T09:00");
        let s = r.split_at("2026-03-01T09:00", now()).expect("splits");
        assert!(s.ended.archived);
        assert_eq!(s.ended.rrule.until, None);
    }

    #[test]
    fn a_split_carries_what_is_left_of_a_count() {
        let r = routine("FREQ=DAILY;COUNT=10", "2026-03-01T09:00");
        let s = r.split_at("2026-03-04T09:00", now()).unwrap();
        assert_eq!(s.continuation.rrule.count, Some(7));
    }

    #[test]
    fn a_split_at_a_key_the_rule_does_not_produce_is_refused() {
        let r = routine("FREQ=WEEKLY;BYDAY=MO", "2026-03-02T09:00");
        assert_eq!(
            r.split_at("2026-03-04T09:00", now()),
            Err(SplitError::NotAnOccurrence("2026-03-04T09:00".into()))
        );
        assert!(matches!(
            r.split_at("tomorrow", now()),
            Err(SplitError::BadKey(_))
        ));
        assert!(r.split_at("2026-03-09T09:00", now()).is_ok());
    }

    #[test]
    fn the_latest_anchor_at_or_before_a_key_owns_it() {
        let r = routine("FREQ=DAILY", "2026-03-01T09:00");
        let a = r.split_at("2026-03-04T09:00", now()).unwrap().continuation;
        let b = r.split_at("2026-03-07T09:00", now()).unwrap().continuation;
        let chain = vec![r.clone(), a.clone(), b.clone()];
        assert!(r.owns_key("2026-03-02T09:00", &chain));
        assert!(!a.owns_key("2026-03-02T09:00", &chain));
        assert!(a.owns_key("2026-03-05T09:00", &chain));
        assert!(!r.owns_key("2026-03-05T09:00", &chain));
        assert!(b.owns_key("2026-03-08T09:00", &chain));
        assert!(!a.owns_key("2026-03-08T09:00", &chain));
        // A deleted continuation hands its keys back.
        let mut gone = b;
        gone.deleted = true;
        let chain = vec![r, a.clone(), gone];
        assert!(a.owns_key("2026-03-08T09:00", &chain));
    }

    fn ids(n: u8) -> Vec<EntityRef> {
        (1..=n)
            .map(|i| EntityRef::new(EntityKind::Task, [i; 16]))
            .collect()
    }

    #[test]
    fn skip_lapses_every_past_open_occurrence() {
        let v = catchup_view(&RoutineCatchupPolicy::Skip, &ids(3));
        assert_eq!(v.lapsed.len(), 3);
        assert!(v.merged.is_empty());
    }

    #[test]
    fn merge_shows_the_latest_for_all_of_them() {
        let t = ids(3);
        let v = catchup_view(&RoutineCatchupPolicy::Merge, &t);
        assert_eq!(v.lapsed, t[..2].iter().copied().collect());
        assert_eq!(v.merged.get(&t[2]), Some(&3));
        let one = catchup_view(&RoutineCatchupPolicy::Merge, &t[..1]);
        assert_eq!(one, CatchupView::default());
    }

    #[test]
    fn queue_and_an_unknown_policy_show_every_one() {
        assert_eq!(
            catchup_view(&RoutineCatchupPolicy::Queue, &ids(3)),
            CatchupView::default()
        );
        let unknown = RoutineCatchupPolicy::from_raw("later");
        assert_eq!(catchup_view(&unknown, &ids(3)), CatchupView::default());
    }
}
