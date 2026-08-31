//! Stable identities for entities imported from an external calendar.
//!
//! `docs/09-integrations/icalendar.md` §Import states the dedup rule: a
//! re-import "dedups by `(import_source_id, uid)`", where the same UID from
//! two sources is two Blocks and the same UID from the same source is an
//! **update** of the one Block it already made.
//!
//! Nothing in the v1 schema stores an `external_id` — `docs/02-domain/
//! time-blocks.md` §"Specified but not modelled" lists it as landing later —
//! so the pair has to be carried somewhere that already exists. It is carried
//! in the **entity id itself**: [`imported_block_id`] hashes
//! `(source, uid)` into the Block's 16 bytes, exactly as
//! [`crate::routine_gen::occurrence_task_id`] hashes `(routine, occurrence)`
//! into a materialized Task's.
//!
//! Two properties fall out of that, and both are the point:
//!
//! * **Idempotence.** A second import of the same file computes the same id,
//!   so the write lands on the Block already there instead of minting a new
//!   one. There is no ledger to keep in step with the vault, and nothing to
//!   repair if one is lost.
//! * **Convergence.** Two devices importing the same file *independently*
//!   compute the same id too, so their ops merge under the ordinary
//!   entity-level LWW rule (ADR-0014) rather than leaving the user with two
//!   copies of every event. A device-local dedup table could not do this.
//!
//! The derived id is not a ULID: its high 48 bits are hash output, not a
//! timestamp. Nothing in the vault reads an id as a clock — Blocks are ordered
//! by `starts_at_ms`, and `created_at` is its own column — so the only thing
//! lost is that an imported Block's id does not sort by creation time.
//!
//! # Round-tripping Sunrise's own export
//!
//! An exported `.ics` has to carry a `UID`, and re-importing the file Sunrise
//! just wrote must not produce a second copy of every Block. So export emits
//! [`block_uid`] — the Block's own id with a host part — and
//! [`imported_block_id`] recognises that form and returns the id it names
//! rather than hashing it. Export → import is then the identity, for any
//! `source`.
//!
//! The host part is under `.invalid`, which RFC 2606 §2 reserves precisely so
//! that a name which is never meant to resolve cannot collide with a real one.
//! A UID only has to be globally unique, and the Block id already is.

use sunrise_id::{EntityKind, EntityRef};

/// Domain separator for [`imported_block_id`]. Versioned: changing the
/// derivation changes which Block an already-imported event maps onto, so a
/// new rule gets a new label rather than silently re-pointing every import.
const DERIVE_LABEL: &[u8] = b"sunrise.import_block.v1";

/// Host part of the `UID` Sunrise emits for a Block it exports.
///
/// `.invalid` is reserved by RFC 2606 §2 for names that must never resolve.
pub const SUNRISE_UID_HOST: &str = "sunrise.invalid";

/// The `UID` to write for `block` when exporting it.
#[must_use]
pub fn block_uid(block: EntityRef) -> String {
    format!("{}@{SUNRISE_UID_HOST}", block.to_str())
}

/// The Block a `UID` names, when that `UID` is one Sunrise itself emitted.
///
/// `None` for every other UID, including a well-formed id under a different
/// host: another calendar's `blk_…@example.com` is that calendar's identifier
/// and must not be read as a claim on a local entity.
#[must_use]
pub fn uid_to_block(uid: &str) -> Option<EntityRef> {
    let local = uid.strip_suffix(SUNRISE_UID_HOST)?.strip_suffix('@')?;
    EntityRef::parse(local, EntityKind::Block).ok()
}

/// The Block id that the external item `uid`, seen from `source`, maps onto.
///
/// Deterministic: no clock, no RNG, no vault read. The same pair gives the
/// same id on every device and on every run, which is what makes a re-import
/// an update instead of a duplicate.
///
/// `source` is length-prefixed into the hash so that `("a", "bc")` and
/// `("ab", "c")` cannot collide — a concatenation without it would make the
/// boundary between the two invisible to the hash.
#[must_use]
pub fn imported_block_id(source: &str, uid: &str) -> EntityRef {
    if let Some(local) = uid_to_block(uid) {
        return local;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(DERIVE_LABEL);
    hasher.update(&(source.len() as u64).to_be_bytes());
    hasher.update(source.as_bytes());
    hasher.update(uid.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityRef::new(EntityKind::Block, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_pair_always_derives_the_same_id() {
        let a = imported_block_id("ics", "ev1@example.com");
        let b = imported_block_id("ics", "ev1@example.com");
        assert_eq!(a, b);
        assert_eq!(a.kind(), EntityKind::Block);
    }

    /// The spec's rule, stated directly: same UID, two sources, two Blocks.
    #[test]
    fn the_same_uid_from_two_sources_is_two_blocks() {
        assert_ne!(
            imported_block_id("ics", "ev1@example.com"),
            imported_block_id("work-calendar", "ev1@example.com")
        );
    }

    #[test]
    fn different_uids_from_one_source_are_different_blocks() {
        assert_ne!(
            imported_block_id("ics", "ev1@example.com"),
            imported_block_id("ics", "ev2@example.com")
        );
    }

    /// The length prefix earns its place: without it these two pairs would
    /// hash the same bytes.
    #[test]
    fn the_source_boundary_is_hashed() {
        assert_ne!(imported_block_id("a", "bc"), imported_block_id("ab", "c"));
    }

    #[test]
    fn exported_uids_round_trip_to_the_block_they_name() {
        let id = EntityRef::new(EntityKind::Block, [9u8; 16]);
        let uid = block_uid(id);
        assert!(uid.ends_with("@sunrise.invalid"), "got {uid}");
        assert_eq!(uid_to_block(&uid), Some(id));
        // And the whole point: exporting then re-importing is the identity,
        // whatever source the re-import is filed under.
        assert_eq!(imported_block_id("ics", &uid), id);
        assert_eq!(imported_block_id("anything-else", &uid), id);
    }

    /// A well-formed id under someone else's host is *their* identifier.
    #[test]
    fn a_foreign_host_is_not_a_claim_on_a_local_block() {
        let id = EntityRef::new(EntityKind::Block, [9u8; 16]);
        let foreign = format!("{}@example.com", id.to_str());
        assert_eq!(uid_to_block(&foreign), None);
        assert_ne!(imported_block_id("ics", &foreign), id);
    }

    #[test]
    fn a_malformed_sunrise_uid_falls_back_to_hashing() {
        assert_eq!(uid_to_block("not-an-id@sunrise.invalid"), None);
        // A task id is not a block id, however well-formed.
        let task = EntityRef::new(EntityKind::Task, [1u8; 16]);
        assert_eq!(
            uid_to_block(&format!("{}@sunrise.invalid", task.to_str())),
            None
        );
    }
}
