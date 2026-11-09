//! Block (time-block) entity per `docs/02-domain/time-blocks.md`.
//!
//! A Block is a scheduled time range that may bind to 0..N Tasks, through its
//! `tasks` OR-set. A Task's `blocks` is derived from those sets on read.
//!
//! # Title
//!
//! A Block's title is a **shadow copy** of the bound Task's title taken at
//! bind time, not a live binding — so renaming the Task later does not rewrite
//! the calendar. [`Block::title_track_task`] opts one Block back into live
//! tracking, and [`Block::resolve_title`] is the single place that decides
//! which of the two a reader gets. Per the spec a Block bound to two or more
//! Tasks ignores the flag: there is no single task title to shadow.
//!
//! # Kind, series and import fields
//!
//! `kind`, `rrule`, `exceptions`, `split_from`, `location`, `notes`, `source`
//! and `external_id` are the fields of `docs/02-domain/time-blocks.md`
//! §Fields beyond the base ones (#1147). Each is absent from the wire when
//! unset, so a Block that sets none encodes exactly as it did before them.
//! [`Block::effective_kind`] is the kind logic reads, by the §Migration
//! default rule; [`Block::series`] is the series a block with an `rrule`
//! holds, which [`expand_block`] expands.

mod conflict;
mod series;

pub use conflict::{merge_blocks, overlaps, BlockOverlap};
pub use series::{
    expand_block, split_block_id, BlockException, BlockOccurrence, BlockSeries, BlockSeriesError,
    BlockSeriesSplit,
};

use crate::common::NoteBody;
use crate::rrule::RRule;
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{
    validate_title, ValidationError, MAX_BLOCK_LOCATION_LEN, MAX_BLOCK_TITLE_LEN,
};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

/// Whether the planner may move a Block (`docs/02-domain/time-blocks.md`
/// §Fixed and flexible, ADR-0048).
///
/// An unrecognised kind reads as [`BlockKind::Fixed`], the safe answer, and
/// is written back verbatim: a kind this build does not know must not let
/// the planner move a block somebody committed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// A commitment with other people or the world; only the user moves it.
    Fixed,
    /// Time the user reserved for themselves; the planner may move it.
    Flexible,
    /// A kind this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(BlockKind, fallback = Fixed, {
    Fixed => "fixed",
    Flexible => "flexible",
});

/// Where a Block came from (`docs/02-domain/time-blocks.md` §Fields).
///
/// An unrecognised source is kept verbatim. It has no fallback: the only
/// thing a source decides is the kind an absent `kind` reads as, and
/// [`Block::effective_kind`] reads every source but `sunrise` as fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockSource {
    /// Created in Sunrise.
    Sunrise,
    /// Imported from a one-shot `.ics` file.
    ImportIcs,
    /// A source this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(BlockSource, {
    Sunrise => "sunrise",
    ImportIcs => "import:ics",
});

/// Persisted Block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Block id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Owning Stream, or `None` for a block that belongs to no stream
    /// (ADR-0046 §1), sealed under the private key domain and drawn in the
    /// neutral palette colour. A projected block carries its effective stream
    /// ([`crate::effective_stream`]); on the wire `None` is the legacy Inbox
    /// reference ([`crate::inbox::wire`]).
    #[serde(default, with = "crate::inbox::wire")]
    pub stream_id: Option<EntityRef>,
    /// Start time. See [`SunriseTime`]: a block at "09:00" is a different
    /// commitment from a block at a fixed instant, and a device that changes
    /// zone must move one and not the other.
    pub starts_at: SunriseTime,
    /// End time. MUST resolve after `starts_at`.
    pub ends_at: SunriseTime,
    /// Optional title (free text; UI may compose from bound tasks).
    #[serde(default)]
    pub title: Option<String>,
    /// Recompute the title from the single bound Task's *current* title on
    /// read, instead of keeping the shadow copy taken at bind time.
    ///
    /// Defaults to `false` so a user-edited Block title is never silently
    /// overwritten, per `docs/02-domain/time-blocks.md` §Block title. Ignored
    /// when the Block binds two or more Tasks.
    #[serde(default)]
    pub title_track_task: bool,
    /// Tasks bound to this Block (OR-Set).
    #[serde(default)]
    pub tasks: BTreeSet<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Whether the planner may move the block. Absent on a block written
    /// before the field existed; [`Self::effective_kind`] reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BlockKind>,
    /// The rule of a recurring block, whose `starts_at` and `ends_at` are
    /// then the series anchor (§Recurring blocks). Absent for a one-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<RRule>,
    /// Per-occurrence changes of a recurring block, keyed by occurrence key:
    /// a map of registers, one per key (ADR-0044), so two devices changing
    /// two occurrences both keep their change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exceptions: BTreeMap<String, BlockException>,
    /// The series root this block continues, for a block split off by an
    /// edit *this and future* ([`split_block_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_from: Option<EntityRef>,
    /// Free-text location, at most [`MAX_BLOCK_LOCATION_LEN`] characters;
    /// never matched to a Place automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<NoteBody>,
    /// Where the block came from. Absent reads as created in Sunrise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BlockSource>,
    /// The foreign id an `.ics` import read, carried back out on export.
    /// Never the dedup key (§`external_id` is not the dedup key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl Block {
    /// The kind logic acts on, always a known arm.
    ///
    /// A stored kind reads as itself, an unknown one as
    /// [`BlockKind::FALLBACK`]. An absent kind reads by the §Migration
    /// default rule of `docs/02-domain/time-blocks.md`: flexible when
    /// `source` is absent or `sunrise` (a user-created block), fixed for any
    /// other source.
    #[must_use]
    pub fn effective_kind(&self) -> BlockKind {
        match (&self.kind, &self.source) {
            (Some(kind), _) => kind.effective(),
            (None, None | Some(BlockSource::Sunrise)) => BlockKind::Flexible,
            (None, Some(_)) => BlockKind::Fixed,
        }
    }

    /// The series a recurring block holds: its anchor range, rule and
    /// exceptions, for [`expand_block`]. `None` for a block with no `rrule`.
    #[must_use]
    pub fn series(&self) -> Option<BlockSeries> {
        Some(BlockSeries {
            starts_at: self.starts_at.clone(),
            ends_at: self.ends_at.clone(),
            rrule: self.rrule.clone()?,
            exceptions: self.exceptions.clone(),
        })
    }

    /// Re-check the Block's cross-field invariants.
    ///
    /// `ends_at` must resolve strictly after `starts_at` for a reader in `tz`
    /// (`docs/10-cross-cutting/time.md` §2 rule 1).
    ///
    /// A recurring block's anchor must also be one its series can repeat
    /// ([`BlockSeries::validate`]), and its location and every exception's
    /// title and location must fit their bounds.
    pub fn validate_invariants(&self, tz: &TimeZone) -> Result<(), ValidationError> {
        validate_range(&self.starts_at, &self.ends_at, tz)?;
        if let Some(t) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        validate_location(self.location.as_deref(), "block.location")?;
        if let Some(series) = self.series() {
            series.validate().map_err(|e| match e {
                BlockSeriesError::Anchor => ValidationError::Field {
                    field: "block.starts_at",
                    constraint: "zoned_or_floating",
                },
                _ => ValidationError::Field {
                    field: "block.ends_at",
                    constraint: "same_kind_as_starts_at",
                },
            })?;
        }
        for exception in self.exceptions.values() {
            if let Some(t) = &exception.title {
                let _ = validate_title(t, "block.exceptions.title", MAX_BLOCK_TITLE_LEN)?;
            }
            validate_location(exception.location.as_deref(), "block.exceptions.location")?;
        }
        Ok(())
    }

    /// The title a reader should show, given the current titles of the bound
    /// Tasks in `bound_titles` (in `tasks` order).
    ///
    /// Three cases, in the order the spec states them:
    ///
    /// 1. Two or more bound Tasks — `title_track_task` is ignored and the
    ///    stored title stands, because there is no single task to shadow.
    /// 2. Exactly one bound Task and `title_track_task` — the Task's current
    ///    title wins over the stored shadow copy.
    /// 3. Otherwise — the stored title, which for a freshly bound Block *is*
    ///    the shadow copy taken at bind time.
    #[must_use]
    pub fn resolve_title<'a>(&'a self, bound_titles: &'a [String]) -> Option<&'a str> {
        if self.title_track_task && bound_titles.len() == 1 {
            return Some(bound_titles[0].as_str());
        }
        self.title.as_deref()
    }
}

/// Draft for creating a Block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDraft {
    /// Owning Stream; `None` (or the legacy Inbox reference) for none.
    #[serde(default)]
    pub stream_id: Option<EntityRef>,
    /// Start.
    pub starts_at: SunriseTime,
    /// End.
    pub ends_at: SunriseTime,
    /// Optional title. Left `None` with exactly one task bound, the core takes
    /// the shadow copy from that task's title.
    pub title: Option<String>,
    /// Track the single bound Task's title instead of shadow-copying it.
    #[serde(default)]
    pub title_track_task: bool,
    /// Optional tasks to bind on creation.
    pub tasks: Vec<EntityRef>,
}

impl BlockDraft {
    /// Validate the draft against domain rules, resolving its bounds in the
    /// reader's zone `tz`.
    pub fn validate(&self, tz: &TimeZone) -> Result<(), ValidationError> {
        validate_range(&self.starts_at, &self.ends_at, tz)?;
        if let Some(t) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        // A Block over two or more Tasks has no single title to shadow, so an
        // explicit one is required rather than silently produced.
        if self.title.is_none() && self.tasks.len() > 1 {
            return Err(ValidationError::Field {
                field: "block.title",
                constraint: "required_for_multi_task",
            });
        }
        Ok(())
    }
}

/// Patch applied via `Command::UpdateBlock`. `Some(None)` clears an optional
/// field; `None` leaves it unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockPatch {
    /// New start.
    pub starts_at: Option<SunriseTime>,
    /// New end.
    pub ends_at: Option<SunriseTime>,
    /// New title; `Some(None)` clears it.
    pub title: Option<Option<String>>,
    /// Turn live title tracking on or off.
    pub title_track_task: Option<bool>,
    /// Move the Block to another Stream, or (`Some(None)`) to no stream.
    pub stream_id: Option<Option<EntityRef>>,
}

impl BlockPatch {
    /// Validate the patch's individual fields. Cross-field rules are re-checked
    /// on the patched Block by [`Block::validate_invariants`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(Some(t)) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        Ok(())
    }
}

/// A location is free text of at most [`MAX_BLOCK_LOCATION_LEN`] characters.
fn validate_location(location: Option<&str>, field: &'static str) -> Result<(), ValidationError> {
    if location.is_some_and(|l| l.chars().count() > MAX_BLOCK_LOCATION_LEN) {
        return Err(ValidationError::Field {
            field,
            constraint: "max_length",
        });
    }
    Ok(())
}

/// `ends_at` must resolve strictly after `starts_at`, both resolved in `tz`.
///
/// A kind this build cannot place on the timeline is compared with nothing:
/// its stand-in key would invent a violation.
fn validate_range(
    starts_at: &SunriseTime,
    ends_at: &SunriseTime,
    tz: &TimeZone,
) -> Result<(), ValidationError> {
    if matches!(
        (ends_at.resolve_in(tz), starts_at.resolve_in(tz)),
        (Some(e), Some(s)) if e <= s
    ) {
        return Err(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil;
    use sunrise_id::EntityKind;

    fn stream() -> EntityRef {
        EntityRef::new(EntityKind::Stream, [7u8; 16])
    }

    fn task(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }

    fn at(hour: i8) -> SunriseTime {
        SunriseTime::floating(civil::date(2026, 3, 4).at(hour, 0, 0, 0))
    }

    fn draft() -> BlockDraft {
        BlockDraft {
            stream_id: Some(stream()),
            starts_at: at(9),
            ends_at: at(10),
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: Vec::new(),
        }
    }

    fn block() -> Block {
        Block {
            id: EntityRef::new(EntityKind::Block, [1u8; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: Some(stream()),
            starts_at: at(9),
            ends_at: at(10),
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: BTreeSet::new(),
            deleted: false,
            kind: None,
            rrule: None,
            exceptions: BTreeMap::new(),
            split_from: None,
            location: None,
            notes: None,
            source: None,
            external_id: None,
            unknown: Unknowns::new(),
        }
    }

    #[test]
    fn draft_accepts_a_forward_range() {
        draft().validate(&TimeZone::UTC).unwrap();
    }

    #[test]
    fn draft_rejects_an_inverted_range() {
        let d = BlockDraft {
            starts_at: at(10),
            ends_at: at(9),
            ..draft()
        };
        assert_eq!(
            d.validate(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at"
            })
        );
    }

    #[test]
    fn draft_rejects_a_zero_length_range() {
        let d = BlockDraft {
            starts_at: at(9),
            ends_at: at(9),
            ..draft()
        };
        assert_eq!(
            d.validate(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at"
            })
        );
    }

    /// A range is compared on the storage index key, so mixed kinds order the
    /// same way here as they do in `ORDER BY starts_at_ms`.
    #[test]
    fn range_compares_across_kinds() {
        let d = BlockDraft {
            starts_at: SunriseTime::instant(
                Timestamp::from_millisecond(1_772_000_000_000).unwrap(),
            ),
            ends_at: SunriseTime::floating(civil::date(2000, 1, 1).at(0, 0, 0, 0)),
            ..draft()
        };
        assert_eq!(
            d.validate(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at"
            })
        );
    }

    #[test]
    fn draft_requires_a_title_for_a_multi_task_block() {
        let d = BlockDraft {
            title: None,
            tasks: vec![task(1), task(2)],
            ..draft()
        };
        assert_eq!(
            d.validate(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.title",
                constraint: "required_for_multi_task"
            })
        );
        // One task is fine: the core shadow-copies that task's title.
        let one = BlockDraft {
            title: None,
            tasks: vec![task(1)],
            ..draft()
        };
        one.validate(&TimeZone::UTC).unwrap();
    }

    #[test]
    fn draft_rejects_an_over_long_title() {
        let d = BlockDraft {
            title: Some("x".repeat(MAX_BLOCK_TITLE_LEN + 1)),
            ..draft()
        };
        assert_eq!(
            d.validate(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.title",
                constraint: "max_length"
            })
        );
    }

    #[test]
    fn shadow_copy_is_the_default() {
        let mut b = block();
        b.tasks.insert(task(1));
        assert_eq!(
            b.resolve_title(&["Renamed later".to_string()]),
            Some("Deep work")
        );
    }

    #[test]
    fn tracking_follows_the_single_bound_task() {
        let mut b = block();
        b.title_track_task = true;
        b.tasks.insert(task(1));
        assert_eq!(
            b.resolve_title(&["Renamed later".to_string()]),
            Some("Renamed later")
        );
    }

    #[test]
    fn tracking_is_ignored_once_a_second_task_is_bound() {
        let mut b = block();
        b.title_track_task = true;
        b.tasks.insert(task(1));
        b.tasks.insert(task(2));
        assert_eq!(
            b.resolve_title(&["One".to_string(), "Two".to_string()]),
            Some("Deep work")
        );
    }

    #[test]
    fn tracking_with_no_bound_task_falls_back_to_the_stored_title() {
        let mut b = block();
        b.title_track_task = true;
        assert_eq!(b.resolve_title(&[]), Some("Deep work"));
    }

    #[test]
    fn invariants_reject_an_inverted_range() {
        let b = Block {
            starts_at: at(11),
            ends_at: at(10),
            ..block()
        };
        assert_eq!(
            b.validate_invariants(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at"
            })
        );
    }

    /// A time kind this build cannot place is compared with nothing.
    #[test]
    fn invariants_accept_an_unplaceable_bound() {
        let unknown = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: crate::Unknowns::new(),
        };
        let b = Block {
            starts_at: unknown.clone(),
            ..block()
        };
        b.validate_invariants(&TimeZone::UTC).unwrap();
        let b = Block {
            ends_at: unknown,
            ..block()
        };
        b.validate_invariants(&TimeZone::UTC).unwrap();
    }

    /// A block that sets none of the #1147 fields writes none of their keys,
    /// so it encodes exactly as it did before them.
    #[test]
    fn unset_series_and_import_fields_are_absent_from_the_wire() {
        let v = ciborium::Value::serialized(&block()).unwrap();
        let keys: Vec<&str> = v
            .as_map()
            .unwrap()
            .iter()
            .filter_map(|(k, _)| k.as_text())
            .collect();
        for absent in [
            "kind",
            "rrule",
            "exceptions",
            "split_from",
            "location",
            "notes",
            "source",
            "external_id",
        ] {
            assert!(!keys.contains(&absent), "{absent} is written");
        }
    }

    /// The fields round-trip, an unknown kind and source verbatim.
    #[test]
    fn series_and_import_fields_round_trip() {
        let b = Block {
            kind: Some(BlockKind::from_raw("tentative")),
            rrule: Some(RRule::parse("FREQ=DAILY;COUNT=3").unwrap()),
            exceptions: BTreeMap::from([
                ("2026-03-05T09:00".to_owned(), BlockException::cancelled()),
                (
                    "2026-03-06T09:00".to_owned(),
                    BlockException {
                        starts_at: Some(at(11)),
                        title: Some("Later".into()),
                        ..BlockException::default()
                    },
                ),
            ]),
            split_from: Some(EntityRef::new(EntityKind::Block, [9u8; 16])),
            location: Some("Room 4".into()),
            notes: Some(NoteBody(b"agenda".to_vec())),
            source: Some(BlockSource::from_raw("import:caldav")),
            external_id: Some("uid@example.com".into()),
            ..block()
        };
        let bytes = sunrise_cbor::encode_canonical(&b).unwrap();
        let back: Block = sunrise_cbor::decode_canonical(&bytes).unwrap();
        assert_eq!(back, b);
    }

    /// §Migration default: an absent kind is flexible for a block created in
    /// Sunrise and fixed for one from anywhere else; a stored kind wins, and
    /// an unknown one reads as fixed.
    #[test]
    fn the_effective_kind_follows_the_migration_default() {
        let with = |kind: Option<BlockKind>, source: Option<BlockSource>| {
            Block {
                kind,
                source,
                ..block()
            }
            .effective_kind()
        };
        assert_eq!(with(None, None), BlockKind::Flexible);
        assert_eq!(with(None, Some(BlockSource::Sunrise)), BlockKind::Flexible);
        assert_eq!(with(None, Some(BlockSource::ImportIcs)), BlockKind::Fixed);
        assert_eq!(
            with(None, Some(BlockSource::from_raw("import:caldav"))),
            BlockKind::Fixed
        );
        assert_eq!(
            with(Some(BlockKind::Flexible), Some(BlockSource::ImportIcs)),
            BlockKind::Flexible
        );
        assert_eq!(with(Some(BlockKind::Fixed), None), BlockKind::Fixed);
        assert_eq!(
            with(Some(BlockKind::from_raw("tentative")), None),
            BlockKind::Fixed
        );
    }

    /// A block with a rule holds a series that `expand_block` expands; one
    /// without holds none.
    #[test]
    fn a_recurring_block_holds_its_series() {
        assert_eq!(block().series(), None);
        let b = Block {
            rrule: Some(RRule::parse("FREQ=DAILY;COUNT=3").unwrap()),
            exceptions: BTreeMap::from([(
                "2026-03-05T09:00".to_owned(),
                BlockException::cancelled(),
            )]),
            ..block()
        };
        let series = b.series().unwrap();
        assert_eq!(series.starts_at, b.starts_at);
        assert_eq!(series.ends_at, b.ends_at);
        let window = (
            Timestamp::from_second(1_772_000_000).unwrap(),
            Timestamp::from_second(1_773_000_000).unwrap(),
        );
        let keys: Vec<String> = expand_block(&series, window, &TimeZone::UTC)
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, ["2026-03-04T09:00", "2026-03-06T09:00"]);
    }

    #[test]
    fn invariants_check_the_series_anchor_and_the_new_bounds() {
        let instant = Block {
            starts_at: SunriseTime::instant(Timestamp::from_second(1_772_600_000).unwrap()),
            ends_at: SunriseTime::instant(Timestamp::from_second(1_772_603_600).unwrap()),
            rrule: Some(RRule::parse("FREQ=DAILY").unwrap()),
            ..block()
        };
        assert_eq!(
            instant.validate_invariants(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.starts_at",
                constraint: "zoned_or_floating"
            })
        );
        let long = Block {
            location: Some("x".repeat(MAX_BLOCK_LOCATION_LEN + 1)),
            ..block()
        };
        assert_eq!(
            long.validate_invariants(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.location",
                constraint: "max_length"
            })
        );
        let exception = Block {
            rrule: Some(RRule::parse("FREQ=DAILY").unwrap()),
            exceptions: BTreeMap::from([(
                "2026-03-05T09:00".to_owned(),
                BlockException {
                    title: Some("x".repeat(MAX_BLOCK_TITLE_LEN + 1)),
                    ..BlockException::default()
                },
            )]),
            ..block()
        };
        assert_eq!(
            exception.validate_invariants(&TimeZone::UTC),
            Err(ValidationError::Field {
                field: "block.exceptions.title",
                constraint: "max_length"
            })
        );
    }

    #[test]
    fn patch_validates_its_title() {
        let p = BlockPatch {
            title: Some(Some("  ".into())),
            ..Default::default()
        };
        assert_eq!(p.validate(), Err(ValidationError::InvalidTitle));
        let clear = BlockPatch {
            title: Some(None),
            ..Default::default()
        };
        clear.validate().unwrap();
    }
}
