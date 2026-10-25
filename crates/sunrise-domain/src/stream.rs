//! Stream entity per `docs/02-domain/streams.md`.

use crate::common::NoteBody;
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError, MAX_STREAM_NAME_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sunrise_id::EntityRef;

/// Fixed Stream colors per the spec's palette. Currently 8 fixed values.
///
/// An unrecognised color reads as [`StreamColor::Slate`] and is written back
/// verbatim, on the wire and in storage alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamColor {
    /// Default neutral.
    Slate,
    /// Reds.
    Rose,
    /// Oranges.
    Amber,
    /// Greens.
    Emerald,
    /// Blues.
    Sky,
    /// Indigos.
    Indigo,
    /// Purples.
    Violet,
    /// Pinks.
    Pink,
    /// A color this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(StreamColor, fallback = Slate, {
    Slate => "slate",
    Rose => "rose",
    Amber => "amber",
    Emerald => "emerald",
    Sky => "sky",
    Indigo => "indigo",
    Violet => "violet",
    Pink => "pink",
});

/// Review cadence for a Stream.
///
/// An unrecognised cadence reads as [`StreamReviewCadence::Weekly`], the
/// documented default, and is written back verbatim. Reading it as `None`
/// would make a Stream silently stop appearing in reviews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamReviewCadence {
    /// Weekly review.
    Weekly,
    /// Every two weeks.
    Biweekly,
    /// Monthly review.
    Monthly,
    /// No review reminders.
    None,
    /// A cadence this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(StreamReviewCadence, fallback = Weekly, {
    Weekly => "weekly",
    Biweekly => "biweekly",
    Monthly => "monthly",
    None => "none",
});

/// Persisted Stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Stream id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-update time.
    pub updated_at: Timestamp,
    /// Display name (1..=128 chars after trim).
    pub name: String,
    /// Optional description (rich text).
    #[serde(default)]
    pub description: Option<NoteBody>,
    /// Color from a fixed palette.
    pub color: StreamColor,
    /// Optional icon id.
    ///
    /// Owned, because `Option<&'static str>` could not be deserialized: a
    /// borrowed `'static` string cannot come out of a deserializer, so the
    /// field carried `#[serde(skip_deserializing)]` and ALWAYS read back as
    /// `None`. Setting an icon therefore survived exactly as long as the
    /// process that set it — it went out on the wire and came back gone, on
    /// every device including the one that set it. That is a data-loss bug
    /// wearing a lifetime annotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional parent Stream — one-level nesting only.
    #[serde(default)]
    pub parent_id: Option<EntityRef>,
    /// Fractional-index sort key: where this Stream sits among its siblings.
    ///
    /// A base-26 string over `A`..=`Z` whose lexicographic order *is* its
    /// numeric order, so a list sorts with `ORDER BY sort_order` and a single
    /// reorder rewrites exactly one row. See [`crate::sort_order`] for the
    /// encoding and [`crate::sort_order::between`] for the arithmetic.
    ///
    /// **Concurrent reorders do not merge.** Under
    /// [ADR-0014](../../../docs/11-adr/0014-entity-level-lww-merge.md) the
    /// whole Stream is one last-writer-wins unit on `(hlc, device_id, seq)`,
    /// and `sort_order` is a field on it like any other — so two devices that
    /// rearrange the same list while apart converge on **one device's whole
    /// Stream row**, not on an interleave of the two arrangements. The
    /// fractional index still earns its place: it keeps one reorder from
    /// rewriting every sibling, which is what makes the losing device's
    /// *other* rows survive. It does not make the two orderings merge, and
    /// nothing here pretends otherwise.
    ///
    /// The empty string means "never ordered" — the column default for a row
    /// that predates the write path — and sorts first, which is where those
    /// rows already were. It is not a valid key: see
    /// [`crate::sort_order::is_valid`].
    pub sort_order: String,
    /// Archived state.
    #[serde(default)]
    pub archived: bool,
    /// Paused state.
    #[serde(default)]
    pub paused: bool,
    /// Optional pause expiry. Any kind (`docs/10-cross-cutting/time.md` §1);
    /// an instant is written as the bare timestamp this field held before
    /// `DOC_SCHEMA_V` 15 ([`crate::time::compat`]), which is the only kind
    /// the stream commands write today, so a stream's wire form is unchanged
    /// for every build.
    #[serde(default, with = "crate::time::compat::option")]
    pub paused_until: Option<SunriseTime>,
    /// Review cadence preference.
    pub review_cadence: StreamReviewCadence,
    /// Optional default Context applied to new tasks captured into this Stream.
    #[serde(default)]
    pub default_context: Option<EntityRef>,
    /// Default reminder lead time for this Stream's Tasks, in seconds.
    ///
    /// Middle of the lead-time hierarchy in
    /// `docs/08-features/notifications.md`: a Task's own value wins, this is
    /// the Stream default, and the device's global default is the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_lead_s: Option<u32>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Where this stream's deletion re-homed its tasks, blocks and routine
    /// templates (ADR-0046 §4): the `MoveTo` target, or `None` for a detach,
    /// a live stream, and a delete by a build before `DOC_SCHEMA_V` 19. An
    /// entity that names a tombstone reads as belonging here
    /// ([`crate::effective_stream`]). Absent on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rehomed_to: Option<EntityRef>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Draft used by the UI when creating a Stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamDraft {
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Color (defaults to `Slate` if omitted).
    pub color: Option<StreamColor>,
    /// Optional parent stream id (one-level nesting only).
    pub parent_id: Option<EntityRef>,
    /// Optional review cadence (defaults to `Weekly`).
    pub review_cadence: Option<StreamReviewCadence>,
    /// Optional default reminder lead time for this Stream's Tasks, seconds.
    pub reminder_lead_s: Option<u32>,
    /// Optional icon name.
    ///
    /// The column and the round-trip have always existed; this field is what
    /// gives them a caller. Without it no command could set an icon, and the
    /// only writer in the tree was a test issuing direct SQL.
    pub icon: Option<String>,
    /// Optional Context applied to Tasks captured into this Stream.
    pub default_context: Option<EntityRef>,
}

impl StreamDraft {
    /// Validate.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.name, "stream.name", MAX_STREAM_NAME_LEN)?;
        Ok(())
    }
}

/// Patch applied via `Command::UpdateStream`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamPatch {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<Option<NoteBody>>,
    /// New color.
    pub color: Option<StreamColor>,
    /// New parent id.
    pub parent_id: Option<Option<EntityRef>>,
    /// New review cadence.
    pub review_cadence: Option<StreamReviewCadence>,
    /// New position among siblings, as a fractional index.
    ///
    /// The whole of the reorder write path: a client computes the key with
    /// [`crate::sort_order::between`] from the two rows the dragged one landed
    /// between, and sends it here. Nothing else moves.
    ///
    /// A reorder is an ordinary Stream update, so it is one entity-level LWW
    /// write and merges like one — see [`Stream::sort_order`].
    pub sort_order: Option<String>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
    /// Pause / unpause.
    pub paused: Option<bool>,
    /// New pause expiry.
    pub paused_until: Option<Option<SunriseTime>>,
    /// New default reminder lead time; `Some(None)` falls back to the device
    /// default.
    pub reminder_lead_s: Option<Option<u32>>,
    /// New icon; `Some(None)` clears it.
    pub icon: Option<Option<String>>,
    /// New default Context; `Some(None)` clears it.
    pub default_context: Option<Option<EntityRef>>,
}

impl StreamPatch {
    /// Validate.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(n) = &self.name {
            let _ = validate_title(n, "stream.name", MAX_STREAM_NAME_LEN)?;
        }
        // Refused here rather than repaired, because there is no honest
        // repair: a key outside `A`..=`Z` has no defined place in the list,
        // and picking one for the user would move a row they did not drag.
        if let Some(k) = &self.sort_order {
            crate::sort_order::validate(k)?;
        }
        Ok(())
    }
}

/// A stream the `parent_id` cycle rule reads as a root (ADR-0065 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedStream<S> {
    /// The stream.
    pub stream: EntityRef,
    /// The parent its `parent_id` names, which no read sees.
    pub parent: EntityRef,
    /// The stamp of that `parent_id` write.
    pub stamp: S,
    /// The cycle, from `stream` along the parents back to `stream`.
    pub cycle: Vec<EntityRef>,
}

/// The parent every stream reads as, and the streams read as roots to make
/// the nesting a forest (ADR-0065 §5).
///
/// Each `parent_id` is a register, so two devices that each nest one stream
/// under the other merge into a loop that neither wrote. In every loop, the
/// stream whose `parent_id` write has the greatest `stamp` (ties by id)
/// reads as a root: the newest move is the one set aside, and the rest of
/// the loop hangs under it. A self-parent is a loop of one.
///
/// A pure function of the `(stream, parent, stamp)` triples, in any order.
/// Nothing is written: the register keeps its value, and moving any stream
/// of the loop elsewhere ends the rule's effect.
#[must_use]
pub fn resolve_parent_cycles<S, I>(
    parents: I,
) -> (BTreeMap<EntityRef, EntityRef>, Vec<RootedStream<S>>)
where
    S: Ord + Clone,
    I: IntoIterator<Item = (EntityRef, EntityRef, S)>,
{
    let all: BTreeMap<EntityRef, (EntityRef, S)> = parents
        .into_iter()
        .map(|(child, parent, stamp)| (child, (parent, stamp)))
        .collect();
    let mut kept: BTreeMap<EntityRef, EntityRef> = all.iter().map(|(c, (p, _))| (*c, *p)).collect();
    let mut rooted = Vec::new();
    // 0 unvisited, 1 on the current walk, 2 done.
    let mut mark: BTreeMap<EntityRef, u8> = BTreeMap::new();
    for start in all.keys() {
        let mut walk = Vec::new();
        let mut at = *start;
        loop {
            match mark.get(&at).copied().unwrap_or(0) {
                0 => {
                    mark.insert(at, 1);
                    walk.push(at);
                    match all.get(&at) {
                        Some((p, _)) => at = *p,
                        None => break,
                    }
                }
                1 => {
                    // `at` closes a loop: the walk from its first visit on.
                    let from = walk.iter().position(|n| *n == at).unwrap_or(0);
                    let members = &walk[from..];
                    if let Some(root) = members
                        .iter()
                        .max_by(|a, b| (&all[*a].1, *a).cmp(&(&all[*b].1, *b)))
                    {
                        let (parent, stamp) = all[root].clone();
                        let pos = members.iter().position(|n| n == root).unwrap_or(0);
                        let mut cycle: Vec<EntityRef> = members[pos..]
                            .iter()
                            .chain(&members[..pos])
                            .copied()
                            .collect();
                        cycle.push(*root);
                        kept.remove(root);
                        rooted.push(RootedStream {
                            stream: *root,
                            parent,
                            stamp,
                            cycle,
                        });
                    }
                    break;
                }
                _ => break,
            }
        }
        for n in walk {
            mark.insert(n, 2);
        }
    }
    rooted.sort_by(|a, b| a.stream.cmp(&b.stream));
    (kept, rooted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: u8) -> EntityRef {
        EntityRef::new(sunrise_id::EntityKind::Stream, [b; 16])
    }

    #[test]
    fn in_a_parent_loop_the_newest_move_reads_as_a_root() {
        // 1 under 2 (older) and 2 under 1 (newer), and 3 under 1.
        let (kept, rooted) =
            resolve_parent_cycles([(s(1), s(2), 5u32), (s(2), s(1), 9), (s(3), s(1), 1)]);
        assert_eq!(kept, BTreeMap::from([(s(1), s(2)), (s(3), s(1))]));
        assert_eq!(rooted.len(), 1);
        assert_eq!(
            (rooted[0].stream, rooted[0].parent, rooted[0].stamp),
            (s(2), s(1), 9)
        );
        assert_eq!(rooted[0].cycle, vec![s(2), s(1), s(2)]);
    }

    #[test]
    fn a_self_parent_is_a_root_and_a_tree_is_untouched() {
        let (kept, rooted) =
            resolve_parent_cycles([(s(1), s(1), 1u32), (s(2), s(3), 2), (s(4), s(3), 3)]);
        assert_eq!(rooted.len(), 1);
        assert_eq!(rooted[0].cycle, vec![s(1), s(1)]);
        assert_eq!(kept, BTreeMap::from([(s(2), s(3)), (s(4), s(3))]));
    }

    #[test]
    fn a_three_loop_entered_from_outside_roots_one_member() {
        // 9 → 1 → 2 → 3 → 1, with 3's move the newest.
        let (kept, rooted) = resolve_parent_cycles([
            (s(9), s(1), 100u32),
            (s(1), s(2), 1),
            (s(2), s(3), 2),
            (s(3), s(1), 3),
        ]);
        assert_eq!(rooted.len(), 1);
        assert_eq!(rooted[0].stream, s(3));
        assert_eq!(rooted[0].cycle, vec![s(3), s(1), s(2), s(3)]);
        assert_eq!(kept.len(), 3);
        // The same input in another order reads the same.
        let (kept2, rooted2) = resolve_parent_cycles([
            (s(3), s(1), 3u32),
            (s(2), s(3), 2),
            (s(9), s(1), 100),
            (s(1), s(2), 1),
        ]);
        assert_eq!((kept, rooted), (kept2, rooted2));
    }

    #[test]
    fn draft_validates_name() {
        let d = StreamDraft {
            name: "Work: Acme".into(),
            ..Default::default()
        };
        d.validate().unwrap();
    }

    #[test]
    fn draft_rejects_empty_name() {
        let d = StreamDraft {
            name: "   ".into(),
            ..Default::default()
        };
        assert_eq!(d.validate(), Err(ValidationError::InvalidTitle));
    }

    #[test]
    fn patch_accepts_a_well_formed_sort_key() {
        let p = StreamPatch {
            sort_order: Some("ABN".into()),
            ..Default::default()
        };
        p.validate().unwrap();
    }

    #[test]
    fn patch_rejects_a_sort_key_it_could_not_have_produced() {
        // The literal every build before the write path hardcoded, the unset
        // sentinel, and a second spelling of a number already in use.
        for bad in ["a0", "", "NA", "N1"] {
            let p = StreamPatch {
                sort_order: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(
                p.validate(),
                Err(ValidationError::Field {
                    field: crate::sort_order::FIELD,
                    constraint: "sort_order_key",
                }),
                "{bad:?} should not be writable"
            );
        }
    }
}
