//! A recurring Block's series (`docs/02-domain/time-blocks.md` §Recurring
//! blocks, #342): the routine series model applied to a time range.
//!
//! A series is an anchor range, a rule and a map of per-occurrence
//! exceptions. [`expand_block`] yields the occurrences that intersect a query
//! window, expanded on read and never stored, through the same expander a
//! routine uses ([`crate::routine_gen::expand`]), so a block and a routine
//! with the same rule and anchor have the same occurrence keys.
//!
//! Pure functions of their arguments, with no clock and no storage: every
//! replica derives the same occurrences, the same split and the same split id
//! from the same data.
//!
//! The wire and storage form of these fields (`Block.rrule`,
//! `Block.exceptions`, `Block.split_from`) is not here yet; a Block carries
//! none of them in this build.

use crate::common::NoteBody;
use crate::routine_gen::{civil_frame_rule, expand, parse_occurrence_key, ExpandError, SplitError};
use crate::rrule::RRule;
use crate::time::SunriseTime;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp};
use std::collections::BTreeMap;
use sunrise_id::{EntityKind, EntityRef};
use thiserror::Error;

/// One occurrence's change (§Recurring blocks, *Exceptions*): either it is
/// cancelled (iCal `EXDATE`), or some of its fields are overridden.
///
/// An exception is one register of the series' exception map, so its fields
/// are written and merged together: two devices changing two different
/// occurrences both keep their change, and two changing the same one resolve
/// to one writer's whole exception.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockException {
    /// The occurrence is removed from the series. The other fields are then
    /// ignored.
    pub cancelled: bool,
    /// The occurrence's new start. Absent keeps the rule's start.
    pub starts_at: Option<SunriseTime>,
    /// The occurrence's new end. Absent keeps the rule's end.
    pub ends_at: Option<SunriseTime>,
    /// The occurrence's title, over the series'.
    pub title: Option<String>,
    /// The occurrence's location, over the series'.
    pub location: Option<String>,
    /// The occurrence's notes, over the series'.
    pub notes: Option<NoteBody>,
}

impl BlockException {
    /// The exception that removes one occurrence (iCal `EXDATE`).
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            cancelled: true,
            ..Self::default()
        }
    }

    /// Whether the exception moves the occurrence on the timeline.
    #[must_use]
    pub const fn moves(&self) -> bool {
        !self.cancelled && (self.starts_at.is_some() || self.ends_at.is_some())
    }
}

/// A recurring Block's series: its anchor range, its rule and its
/// exceptions, keyed by occurrence key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSeries {
    /// The anchor: the first occurrence's start, `zoned` or `floating`
    /// (`docs/10-cross-cutting/time.md` §6).
    pub starts_at: SunriseTime,
    /// The first occurrence's end, of the anchor's kind (and zone). Every
    /// occurrence lasts `ends_at − starts_at` in civil terms.
    pub ends_at: SunriseTime,
    /// The rule.
    pub rrule: RRule,
    /// Per-occurrence changes, keyed by occurrence key.
    pub exceptions: BTreeMap<String, BlockException>,
}

/// One occurrence of a recurring Block, as [`expand_block`] yields it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockOccurrence {
    /// The intended civil start, `YYYY-MM-DDTHH:MM`: the routine occurrence
    /// key, which never depends on how a DST gap resolved.
    pub key: String,
    /// The occurrence's start: of the anchor's kind, or the exception's.
    pub starts_at: SunriseTime,
    /// The occurrence's end: of the anchor's kind, or the exception's.
    pub ends_at: SunriseTime,
    /// The exception applied to this occurrence, when it has one.
    pub exception: Option<BlockException>,
}

/// Why a series cannot be expanded.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockSeriesError {
    /// The anchor is an instant or a whole day, which has no wall-clock time
    /// to repeat (`docs/10-cross-cutting/time.md` §6).
    #[error("a recurring block's start must be zoned or floating")]
    Anchor,
    /// The end is of another kind or zone than the anchor, or not after it.
    #[error("a recurring block's end must be of its start's kind and after it")]
    End,
    /// The rule could not be expanded over the window.
    #[error(transparent)]
    Expand(#[from] ExpandError),
}

/// The anchor of a series, read in civil space.
struct Anchor {
    start: DateTime,
    /// The civil length of every occurrence.
    span: Span,
    /// An upper bound on how long an occurrence lasts on the timeline: the
    /// civil length plus a day for any DST transition inside it.
    reach: SignedDuration,
}

/// A civil value as the UTC instant at the same wall clock: the frame keys
/// are looked up in, which has no DST to move one.
fn civil_utc(civil: DateTime) -> Option<Timestamp> {
    TimeZone::UTC.to_zoned(civil).ok().map(|z| z.timestamp())
}

impl BlockSeries {
    /// Check the series' anchor rules: a `zoned` or `floating` start, and an
    /// end of the same kind, in the same zone, strictly after it in civil
    /// terms.
    ///
    /// # Errors
    /// [`BlockSeriesError::Anchor`] or [`BlockSeriesError::End`].
    pub fn validate(&self) -> Result<(), BlockSeriesError> {
        self.anchor().map(|_| ())
    }

    fn anchor(&self) -> Result<Anchor, BlockSeriesError> {
        let (start, end) = match (&self.starts_at, &self.ends_at) {
            (SunriseTime::Floating { civil: s }, SunriseTime::Floating { civil: e }) => (*s, *e),
            (SunriseTime::Zoned { civil: s, tz: a }, SunriseTime::Zoned { civil: e, tz: b })
                if a == b =>
            {
                (*s, *e)
            }
            (SunriseTime::Floating { .. } | SunriseTime::Zoned { .. }, _) => {
                return Err(BlockSeriesError::End)
            }
            _ => return Err(BlockSeriesError::Anchor),
        };
        if end <= start {
            return Err(BlockSeriesError::End);
        }
        let span = start.until(end).map_err(|_| BlockSeriesError::End)?;
        let (Some(s), Some(e)) = (civil_utc(start), civil_utc(end)) else {
            return Err(BlockSeriesError::End);
        };
        let reach = s
            .duration_until(e)
            .saturating_add(SignedDuration::from_hours(24));
        Ok(Anchor { start, span, reach })
    }

    /// The zone this series' occurrences resolve in for a reader in
    /// `reader`: the anchor's own zone when it is `zoned`, the reader's when
    /// it is `floating`, as for a routine. A zone this build does not know
    /// resolves in the reader's (time.md §3).
    #[must_use]
    pub fn occurrence_zone(&self, reader: &TimeZone) -> TimeZone {
        match &self.starts_at {
            SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).unwrap_or_else(|_| reader.clone()),
            _ => reader.clone(),
        }
    }

    /// The rule for an expansion in the civil-as-UTC frame: its `UNTIL` as
    /// wall clock in the anchor's zone ([`civil_frame_rule`]).
    fn civil_frame_rule(&self) -> RRule {
        let zone = match &self.starts_at {
            SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).ok(),
            _ => None,
        };
        civil_frame_rule(&self.rrule, zone.as_ref())
    }

    /// A wall-clock value as a time of the anchor's kind.
    fn time_of(&self, civil: DateTime) -> SunriseTime {
        match &self.starts_at {
            SunriseTime::Zoned { tz, .. } => SunriseTime::zoned(civil, tz.clone()),
            _ => SunriseTime::floating(civil),
        }
    }

    /// The rule's start and end of the occurrence at wall-clock `civil`.
    fn bounds_of(&self, anchor: &Anchor, civil: DateTime) -> Option<(SunriseTime, SunriseTime)> {
        let end = civil.checked_add(anchor.span).ok()?;
        Some((self.time_of(civil), self.time_of(end)))
    }

    /// The intended wall-clock start of occurrence `key`, when the rule
    /// produces it, cancelled or not.
    #[must_use]
    pub fn occurrence_civil(&self, key: &str) -> Option<DateTime> {
        let anchor = self.anchor().ok()?;
        let at = civil_utc(parse_occurrence_key(key)?)?;
        let end = at.checked_add(SignedDuration::from_mins(1)).ok()?;
        expand(
            &self.civil_frame_rule(),
            anchor.start,
            &TimeZone::UTC,
            (at, end),
        )
        .ok()?
        .into_iter()
        .find(|o| o.key == key)
        .map(|o| o.civil)
    }

    /// Whether the rule produces occurrence `key`, before its exceptions.
    #[must_use]
    pub fn produces_key(&self, key: &str) -> bool {
        self.occurrence_civil(key).is_some()
    }

    /// Edit one occurrence (§Recurring blocks, *Edit scope* "this"): write
    /// `exception` at `key`, replacing any exception already there.
    ///
    /// # Errors
    /// [`SplitError::BadKey`] when `key` is not `YYYY-MM-DDTHH:MM`, and
    /// [`SplitError::NotAnOccurrence`] when the rule does not produce it.
    pub fn except(&mut self, key: &str, exception: BlockException) -> Result<(), SplitError> {
        parse_occurrence_key(key).ok_or_else(|| SplitError::BadKey(key.into()))?;
        if !self.produces_key(key) {
            return Err(SplitError::NotAnOccurrence(key.into()));
        }
        self.exceptions.insert(key.to_owned(), exception);
        Ok(())
    }

    /// Split the series at occurrence `key` (§Recurring blocks, *Edit scope*
    /// "this and future"), as a routine's series splits.
    ///
    /// The ended series keeps every occurrence before `key`: its rule's
    /// `UNTIL` becomes the start of the last one, of the anchor's kind, and
    /// it keeps the exceptions before `key`. It is `None` when `key` is the
    /// first occurrence, which leaves nothing before it. The continuation is
    /// anchored at `key` with the series' length, a `COUNT` reduced by the
    /// occurrences before `key`, and the exceptions at or after it. Its id is
    /// [`split_block_id`] of the series root and `key`.
    ///
    /// # Errors
    /// [`SplitError`] when `key` is not one of this series' occurrences.
    pub fn split_at(&self, key: &str) -> Result<BlockSeriesSplit, SplitError> {
        parse_occurrence_key(key).ok_or_else(|| SplitError::BadKey(key.into()))?;
        let civil = self
            .occurrence_civil(key)
            .ok_or_else(|| SplitError::NotAnOccurrence(key.into()))?;
        let anchor = self
            .anchor()
            .map_err(|_| SplitError::NotAnOccurrence(key.into()))?;
        let (Some(from), Some(to)) = (civil_utc(anchor.start), civil_utc(civil)) else {
            return Err(SplitError::NotAnOccurrence(key.into()));
        };
        let before = expand(
            &self.civil_frame_rule(),
            anchor.start,
            &TimeZone::UTC,
            (from, to),
        )?;

        let ended = before.last().map(|last| {
            let mut ended = self.clone();
            ended.rrule.until = Some(self.time_of(last.civil));
            ended.exceptions.retain(|k, _| k.as_str() < key);
            ended
        });

        let (starts_at, ends_at) = self
            .bounds_of(&anchor, civil)
            .ok_or_else(|| SplitError::NotAnOccurrence(key.into()))?;
        let mut continuation = self.clone();
        continuation.starts_at = starts_at;
        continuation.ends_at = ends_at;
        continuation.rrule.count = self
            .rrule
            .count
            .map(|c| c.saturating_sub(u32::try_from(before.len()).unwrap_or(u32::MAX)));
        continuation.exceptions.retain(|k, _| k.as_str() >= key);
        Ok(BlockSeriesSplit {
            ended,
            continuation,
        })
    }
}

/// The two series a split leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSeriesSplit {
    /// The series split, ended before the key; `None` when the key was its
    /// first occurrence, so the block it belongs to is superseded whole.
    pub ended: Option<BlockSeries>,
    /// The series that continues from the key, before the edit is applied
    /// to it.
    pub continuation: BlockSeries,
}

/// The id of the Block that continues series `root` from occurrence `key`.
///
/// A function of the pair, so two devices that split the same series at the
/// same occurrence create one Block and merge its fields. Its own hash
/// domain, so it never names the routine [`crate::split_routine_id`] derives
/// from the same bytes.
#[must_use]
pub fn split_block_id(root: &EntityRef, key: &str) -> EntityRef {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sunrise.block_split.v1");
    hasher.update(root.bytes());
    hasher.update(key.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityRef::new(EntityKind::Block, bytes)
}

/// Every occurrence of `series` that intersects `window`, for a reader in
/// `reader`, ordered by resolved start, then key.
///
/// Expanded on read, never stored: the rule goes through
/// [`crate::routine_gen::expand`] in civil space from the anchor, in the
/// series' occurrence zone, so the keys are the routine keys and a DST
/// transition moves an occurrence's instant and never its key or its
/// wall-clock length. An occurrence that starts before the window and runs
/// into it is yielded.
///
/// Exceptions apply by key: a cancelled occurrence is not yielded, an
/// override of fields only is yielded at the rule's time with the exception
/// attached, and an override that moves the occurrence is yielded where it
/// moved to, when that intersects the window, wherever the rule put it. A
/// moved occurrence's absent bound keeps the rule's. An exception whose key
/// the rule does not produce applies to nothing.
///
/// A series whose start or end is of a kind this build does not know yields
/// no occurrences (ADR-0045 §6): any schedule guessed for it could be the
/// wrong one. So does a rule holding a value this build does not know
/// ([`crate::routine_gen::expand`]).
///
/// # Errors
/// [`BlockSeriesError::Anchor`] or [`BlockSeriesError::End`] when the series
/// breaks its anchor rules ([`BlockSeries::validate`]), and
/// [`BlockSeriesError::Expand`] when the window is too large to expand.
pub fn expand_block(
    series: &BlockSeries,
    window: (Timestamp, Timestamp),
    reader: &TimeZone,
) -> Result<Vec<BlockOccurrence>, BlockSeriesError> {
    if series.starts_at.is_unknown() || series.ends_at.is_unknown() {
        return Ok(Vec::new());
    }
    let anchor = series.anchor()?;
    let zone = series.occurrence_zone(reader);
    let intersects = |s: &SunriseTime, e: &SunriseTime| {
        matches!(
            (s.resolve_in(reader), e.resolve_in(reader)),
            (Some(s), Some(e)) if s < window.1 && e > window.0
        )
    };

    // An occurrence that starts up to its length before the window can still
    // run into it.
    let from = window.0.checked_sub(anchor.reach).unwrap_or(Timestamp::MIN);
    let mut out = Vec::new();
    for occ in expand(&series.rrule, anchor.start, &zone, (from, window.1))? {
        let exception = series.exceptions.get(&occ.key);
        if exception.is_some_and(|x| x.cancelled || x.moves()) {
            continue;
        }
        let Some((starts_at, ends_at)) = series.bounds_of(&anchor, occ.civil) else {
            continue;
        };
        if intersects(&starts_at, &ends_at) {
            out.push(BlockOccurrence {
                key: occ.key,
                starts_at,
                ends_at,
                exception: exception.cloned(),
            });
        }
    }

    // A moved occurrence lands wherever its exception says, so it is placed
    // from its key rather than found by the window.
    for (key, x) in series.exceptions.iter().filter(|(_, x)| x.moves()) {
        let Some(civil) = series.occurrence_civil(key) else {
            continue;
        };
        let Some((rule_start, rule_end)) = series.bounds_of(&anchor, civil) else {
            continue;
        };
        let starts_at = x.starts_at.clone().unwrap_or(rule_start);
        let ends_at = x.ends_at.clone().unwrap_or(rule_end);
        if intersects(&starts_at, &ends_at) {
            out.push(BlockOccurrence {
                key: key.clone(),
                starts_at,
                ends_at,
                exception: Some(x.clone()),
            });
        }
    }

    out.sort_by(|a, b| {
        a.starts_at
            .cmp_in(&b.starts_at, reader)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(out)
}

#[cfg(test)]
mod tests;
