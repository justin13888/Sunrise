//! Entity kinds and their string prefixes.
//!
//! Per `docs/02-domain/identifiers.md`, every ULID is namespaced by a
//! 4-character ASCII prefix (3 letters + `_`).

use serde::{Deserialize, Serialize};

/// Domain entity kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntityKind {
    /// `tsk_` — Task.
    Task,
    /// `str_` — Stream.
    Stream,
    /// `ctx_` — Context.
    Context,
    /// `rtn_` — Routine.
    Routine,
    /// `blk_` — Block.
    Block,
    /// `not_` — Note.
    Note,
    /// `att_` — Attachment.
    Attachment,
    /// `prs_` — Person.
    Person,
    /// `dev_` — Device.
    Device,
    /// `idn_` — Identity.
    Identity,
    /// `fcs_` — Focus session (append-only; see ADR-0013).
    FocusSession,
    /// `rvw_` — Saved review snapshot (append-only; see
    /// `docs/08-features/reviews-and-stats.md` §Weekly review step 5).
    ReviewSnapshot,
}

impl EntityKind {
    /// Canonical 4-char prefix (3 letters + `_`).
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Task => "tsk_",
            Self::Stream => "str_",
            Self::Context => "ctx_",
            Self::Routine => "rtn_",
            Self::Block => "blk_",
            Self::Note => "not_",
            Self::Attachment => "att_",
            Self::Person => "prs_",
            Self::Device => "dev_",
            Self::Identity => "idn_",
            Self::FocusSession => "fcs_",
            Self::ReviewSnapshot => "rvw_",
        }
    }

    /// Look up an entity kind from its 4-char prefix (including the `_`).
    /// Returns `None` for unknown prefixes.
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "tsk_" => Some(Self::Task),
            "str_" => Some(Self::Stream),
            "ctx_" => Some(Self::Context),
            "rtn_" => Some(Self::Routine),
            "blk_" => Some(Self::Block),
            "not_" => Some(Self::Note),
            "att_" => Some(Self::Attachment),
            "prs_" => Some(Self::Person),
            "dev_" => Some(Self::Device),
            "idn_" => Some(Self::Identity),
            "fcs_" => Some(Self::FocusSession),
            "rvw_" => Some(Self::ReviewSnapshot),
            _ => None,
        }
    }

    /// All kinds, in declaration order.
    #[must_use]
    pub const fn all() -> [Self; 12] {
        [
            Self::Task,
            Self::Stream,
            Self::Context,
            Self::Routine,
            Self::Block,
            Self::Note,
            Self::Attachment,
            Self::Person,
            Self::Device,
            Self::Identity,
            Self::FocusSession,
            Self::ReviewSnapshot,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_prefix() {
        for k in EntityKind::all() {
            assert_eq!(EntityKind::from_prefix(k.prefix()), Some(k));
        }
    }

    #[test]
    fn unknown_prefix_returns_none() {
        assert_eq!(EntityKind::from_prefix("xxx_"), None);
        assert_eq!(EntityKind::from_prefix(""), None);
        assert_eq!(EntityKind::from_prefix("ts"), None);
    }

    #[test]
    fn prefixes_have_canonical_shape() {
        for k in EntityKind::all() {
            let p = k.prefix();
            assert_eq!(p.len(), 4);
            assert!(p.ends_with('_'));
            assert!(p[..3].chars().all(|c| c.is_ascii_lowercase()));
        }
    }

    #[test]
    fn prefixes_are_unique() {
        let prefixes: Vec<&str> = EntityKind::all().iter().map(|k| k.prefix()).collect();
        let mut dedup = prefixes.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(prefixes.len(), dedup.len());
    }
}
