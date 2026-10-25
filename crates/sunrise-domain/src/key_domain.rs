//! The key domains, where a stream's dependents go when it is deleted, and
//! the stream an entity reads as belonging to (ADR-0046 §2, §4).
//!
//! # Three kinds of key domain
//!
//! Every op is sealed under one key domain. [`KeyDomain`] is a closed enum,
//! not a stream id: the vault-meta domain, the private domain that holds
//! stream-less content, and a user-created [`StreamId`]. Only the last can be
//! shared ([`KeyDomain::shareable`]), and a [`StreamId`] can never name
//! either of the others, so "share the private domain" does not type-check.
//!
//! # Deleting a stream re-homes what it held
//!
//! `DeleteStream` writes every live Task, Block and routine template that
//! names the stream to its [`Disposition`]'s target, and records the target
//! on the tombstone (`Stream.rehomed_to`). An entity that names the stream
//! and that the command could not see (written on another device, merged
//! later) is resolved by [`effective_stream`], a pure function of the merged
//! tombstones, so every replica reads it in the same place.

use crate::inbox::{home_stream, names_no_stream, META_DOMAIN_BYTES, PRIVATE_DOMAIN_BYTES};
use sunrise_id::{EntityKind, EntityRef};

/// A user-created Stream: never the vault-meta domain, never the private
/// domain.
///
/// The one way to make one is [`StreamId::new`], which refuses both, so a
/// stream command or a share that takes a `StreamId` cannot be handed either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(EntityRef);

impl StreamId {
    /// `r` as a Stream id, or `None` when it is not a Stream reference or is
    /// a spelling of the meta or private domain.
    #[must_use]
    pub fn new(r: EntityRef) -> Option<Self> {
        (r.kind() == EntityKind::Stream && !names_no_stream(&r)).then_some(Self(r))
    }

    /// The Stream's reference.
    #[must_use]
    pub const fn get(self) -> EntityRef {
        self.0
    }
}

/// The key domain an op is sealed under (ADR-0046 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyDomain {
    /// The vault-meta domain: streams, routines, contexts, saved views,
    /// preferences, and the vault's control ops.
    Meta,
    /// The private domain: tasks and blocks that belong to no stream.
    Private,
    /// A user-created Stream.
    Stream(StreamId),
}

impl KeyDomain {
    /// The domain a Task or Block whose (effective) stream is `stream` seals
    /// under: the stream's, or the private domain for none.
    #[must_use]
    pub fn of_home(stream: Option<EntityRef>) -> Self {
        home_stream(stream)
            .and_then(StreamId::new)
            .map_or(Self::Private, Self::Stream)
    }

    /// The domain named by an envelope's 16-byte stream id.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        match bytes {
            META_DOMAIN_BYTES => Self::Meta,
            PRIVATE_DOMAIN_BYTES => Self::Private,
            other => StreamId::new(EntityRef::new(EntityKind::Stream, other))
                .map_or(Self::Private, Self::Stream),
        }
    }

    /// The 16-byte id an envelope sealed under this domain carries.
    #[must_use]
    pub fn bytes(self) -> [u8; 16] {
        match self {
            Self::Meta => META_DOMAIN_BYTES,
            Self::Private => PRIVATE_DOMAIN_BYTES,
            Self::Stream(s) => *s.get().bytes(),
        }
    }

    /// The Stream this domain is, when it is one: the only domain a share can
    /// carry. The meta and private domains are never shared.
    #[must_use]
    pub const fn shareable(self) -> Option<StreamId> {
        match self {
            Self::Stream(s) => Some(s),
            Self::Meta | Self::Private => None,
        }
    }
}

/// Where a deleted stream's tasks, blocks and routine templates go
/// (`Command::DeleteStream`, ADR-0046 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Disposition {
    /// Every dependent goes to no stream.
    #[default]
    Detach,
    /// Every dependent goes to this live stream.
    MoveTo(EntityRef),
}

impl Disposition {
    /// The stream every dependent is written to: `None` for [`Self::Detach`].
    #[must_use]
    pub const fn target(self) -> Option<EntityRef> {
        match self {
            Self::Detach => None,
            Self::MoveTo(s) => Some(s),
        }
    }
}

/// What a replica holds of a stream an entity names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFate {
    /// Live, or not yet arrived: an entity may name a stream whose create is
    /// still on its way, and it belongs there.
    Live,
    /// A tombstone, with the stream its deletion re-homed its dependents to
    /// (`None` for a detach, or a delete by a build before ADR-0046).
    Deleted {
        /// `Stream.rehomed_to`.
        rehomed_to: Option<EntityRef>,
    },
}

/// The longest chain of re-homing tombstones [`effective_stream`] follows.
pub const MAX_REHOME_CHAIN: usize = 8;

/// The stream an entity whose `stream_id` register holds `stream` belongs to
/// (ADR-0046 §4):
///
/// ```text
/// effective_stream(ref) =
///     ref is None                         → None
///     stream(ref) is live                 → ref
///     stream(ref) is a tombstone          → effective_stream(tombstone.rehomed_to)
///     (a chain longer than 8, or a cycle) → None
/// ```
///
/// A pure function of `fate`, so every replica that holds the same merged
/// tombstones reads every entity in the same stream, and a cycle (two streams
/// each re-homed into the other on two devices) resolves to no stream on all
/// of them. A spelling of "no stream" reads as `None`.
pub fn effective_stream(
    stream: Option<EntityRef>,
    mut fate: impl FnMut(&EntityRef) -> StreamFate,
) -> Option<EntityRef> {
    let mut at = home_stream(stream)?;
    for _ in 0..=MAX_REHOME_CHAIN {
        match fate(&at) {
            StreamFate::Live => return Some(at),
            StreamFate::Deleted { rehomed_to } => at = home_stream(rehomed_to)?,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::inbox_stream_ref;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn s(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [n; 16])
    }

    #[test]
    fn a_stream_id_is_never_the_meta_or_private_domain() {
        assert!(StreamId::new(inbox_stream_ref()).is_none());
        assert!(StreamId::new(EntityRef::new(EntityKind::Stream, META_DOMAIN_BYTES)).is_none());
        assert!(StreamId::new(EntityRef::new(EntityKind::Task, [9; 16])).is_none());
        assert_eq!(StreamId::new(s(9)).map(StreamId::get), Some(s(9)));
    }

    #[test]
    fn a_disposition_names_where_dependents_go() {
        assert_eq!(Disposition::Detach.target(), None);
        assert_eq!(Disposition::MoveTo(s(4)).target(), Some(s(4)));
    }

    #[test]
    fn only_a_stream_domain_is_shareable() {
        assert_eq!(KeyDomain::of_home(None), KeyDomain::Private);
        assert_eq!(
            KeyDomain::of_home(Some(inbox_stream_ref())),
            KeyDomain::Private
        );
        assert_eq!(KeyDomain::Private.shareable(), None);
        assert_eq!(KeyDomain::Meta.shareable(), None);
        let d = KeyDomain::of_home(Some(s(3)));
        assert_eq!(d.shareable().map(StreamId::get), Some(s(3)));
        for d in [KeyDomain::Meta, KeyDomain::Private, d] {
            assert_eq!(KeyDomain::from_bytes(d.bytes()), d);
        }
        assert_eq!(KeyDomain::Private.bytes(), PRIVATE_DOMAIN_BYTES);
    }

    #[test]
    fn effective_stream_follows_the_tombstones() {
        let fates: BTreeMap<EntityRef, StreamFate> = [
            (
                s(1),
                StreamFate::Deleted {
                    rehomed_to: Some(s(2)),
                },
            ),
            (
                s(2),
                StreamFate::Deleted {
                    rehomed_to: Some(s(3)),
                },
            ),
            (s(4), StreamFate::Deleted { rehomed_to: None }),
            (
                s(5),
                StreamFate::Deleted {
                    rehomed_to: Some(s(6)),
                },
            ),
            (
                s(6),
                StreamFate::Deleted {
                    rehomed_to: Some(s(5)),
                },
            ),
        ]
        .into_iter()
        .collect();
        let fate = |r: &EntityRef| fates.get(r).copied().unwrap_or(StreamFate::Live);
        assert_eq!(effective_stream(None, fate), None);
        assert_eq!(effective_stream(Some(inbox_stream_ref()), fate), None);
        assert_eq!(effective_stream(Some(s(3)), fate), Some(s(3)));
        assert_eq!(effective_stream(Some(s(1)), fate), Some(s(3)));
        assert_eq!(effective_stream(Some(s(4)), fate), None, "a detach");
        assert_eq!(effective_stream(Some(s(5)), fate), None, "a cycle");
        assert_eq!(
            effective_stream(Some(s(9)), fate),
            Some(s(9)),
            "not arrived"
        );
    }

    /// A chain of exactly eight tombstones resolves; nine does not.
    #[test]
    fn the_chain_is_bounded_at_eight() {
        // Streams 1..=len are tombstones, each re-homed to the next.
        let chain = |len: u8| {
            move |r: &EntityRef| {
                let n = r.bytes()[0];
                if n <= len {
                    StreamFate::Deleted {
                        rehomed_to: Some(s(n + 1)),
                    }
                } else {
                    StreamFate::Live
                }
            }
        };
        assert_eq!(effective_stream(Some(s(1)), chain(8)), Some(s(9)));
        assert_eq!(effective_stream(Some(s(1)), chain(9)), None);
    }

    proptest! {
        /// The answer is a live stream or none, never a tombstone.
        #[test]
        fn the_answer_is_never_a_tombstone(
            edges in proptest::collection::btree_map(1u8..12, proptest::option::of(1u8..12), 0..12),
            start in 1u8..12,
        ) {
            let fate = |r: &EntityRef| match edges.get(&r.bytes()[0]) {
                Some(to) => StreamFate::Deleted { rehomed_to: to.map(s) },
                None => StreamFate::Live,
            };
            if let Some(got) = effective_stream(Some(s(start)), fate) {
                prop_assert!(!edges.contains_key(&got.bytes()[0]));
            }
        }
    }
}
