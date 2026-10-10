//! Entity kinds and their string prefixes.
//!
//! Per `docs/02-domain/identifiers.md`, every ULID is namespaced by a
//! 4-character ASCII prefix (3 letters + `_`).
//!
//! The kinds, their prefixes and everything else about them are declared once
//! in the entity registry ([`crate::for_each_entity!`]); [`EntityKind`] is
//! generated from it.

pub use crate::registry::EntityKind;

#[cfg(test)]
mod tests {
    use super::*;

    /// The serde names are part of exported JSON; generating the enum must
    /// not move them.
    #[test]
    fn serde_names_are_the_lowercased_variant_names() {
        let names: Vec<String> = EntityKind::all()
            .iter()
            .map(|k| serde_json::to_string(k).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "\"task\"",
                "\"stream\"",
                "\"context\"",
                "\"routine\"",
                "\"block\"",
                "\"note\"",
                "\"attachment\"",
                "\"person\"",
                "\"device\"",
                "\"identity\"",
                "\"focussession\"",
                "\"reviewsnapshot\"",
                "\"preferences\"",
            ]
        );
    }

    /// The prefixes are on every id ever minted.
    #[test]
    fn prefixes_are_pinned() {
        let prefixes: Vec<&str> = EntityKind::all().iter().map(|k| k.prefix()).collect();
        assert_eq!(
            prefixes,
            [
                "tsk_", "str_", "ctx_", "rtn_", "blk_", "not_", "att_", "prs_", "dev_", "idn_",
                "fcs_", "rvw_", "prf_",
            ]
        );
    }

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
