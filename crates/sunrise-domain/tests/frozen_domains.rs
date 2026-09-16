//! The two derived-id domain constants this crate owns, anchored to frozen
//! literals.
//!
//! `sunrise.import_block.v1` decides which Block an already-imported calendar
//! item maps onto, and `sunrise.routine_task.v1` decides which Task a routine
//! occurrence materialises as. Both are computed independently on every
//! replica and never transmitted, so a build that derived either differently
//! duplicates rows across devices while every local test passes.
//!
//! A failure here changes which entity an existing id names. That is a data
//! migration, not a test fix.

use sunrise_crypto_test_vectors::protocol;
use sunrise_domain::{imported_block_id, occurrence_task_id};
use sunrise_id::{EntityKind, EntityRef};

#[test]
fn imported_block_id_vectors_hold() {
    for v in protocol::IMPORT_BLOCK_VECTORS {
        let got = imported_block_id(v.source, v.uid);
        assert_eq!(got.kind(), EntityKind::Block);
        assert_eq!(
            *got.bytes(),
            v.block_id,
            "the sunrise.import_block.v1 derivation drifted for ({:?}, {:?}) — \
             every already-imported item would re-import as a new Block",
            v.source,
            v.uid
        );
    }
}

#[test]
fn occurrence_task_id_vectors_hold() {
    for v in protocol::OCCURRENCE_TASK_ID_VECTORS {
        let routine = EntityRef::new(EntityKind::Routine, v.routine_id);
        let got = occurrence_task_id(&routine, v.key);
        assert_eq!(got.kind(), EntityKind::Task);
        assert_eq!(
            *got.bytes(),
            v.task_id,
            "the sunrise.routine_task.v1 derivation drifted for key {:?} — two \
             replicas would materialise one occurrence as two Tasks",
            v.key
        );
    }
}
