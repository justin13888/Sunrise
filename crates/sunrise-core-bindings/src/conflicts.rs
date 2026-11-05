//! The conflicts of ADR-0065 §4 across the seam: what `CoreQuery::Conflicts`
//! returns, one [`ConflictItem`] per conflicted field.
//!
//! A value crosses as its canonical CBOR, opaque to the client, as `extra`
//! does (ADR-0045 §6): a client shows the field the way it shows the field
//! anyway, and hands a value back to `CoreCommand::ResolveConflict` as the
//! bytes it received. The class and the origin are strings, so a newer
//! build's class reaches an older client as a name it can show rather than a
//! case its `switch` lacks.

use sunrise_core::conflicts::{ConflictClass, ConflictWrite, DeletedEdit, EntityConflict};
use sunrise_core::WriteOrigin;
use sunrise_id::EntityRef;

use crate::dto::hex16;

/// One write to a conflicted field: the winner, or a loser.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConflictWriteItem {
    /// The version that wrote it, `<stream hex>.<device hex>.<seq>`.
    pub version: String,
    /// Its stamp's physical time (epoch ms).
    pub hlc_ms: u64,
    /// Its stamp's logical counter.
    pub hlc_logical: u32,
    /// The writing device, as hex.
    pub device_id: String,
    /// `user` or `generated`.
    pub origin: String,
    /// The value as canonical CBOR; absent when the write left the field
    /// unset.
    pub value: Option<Vec<u8>>,
}

/// A branch that edited a deleted entity without having seen the delete.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConflictEditItem {
    /// The branch's newest version.
    pub version: String,
    /// Its device, as hex.
    pub device_id: String,
    /// The fields that version wrote.
    pub fields: Vec<String>,
}

/// One conflicted field of one entity.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConflictItem {
    /// The entity.
    pub entity: EntityRef,
    /// The field's wire name (`template.title` for a nested one).
    pub field: String,
    /// `tracked`, `state`, `tombstone`, `dependency_cycle`, or with quiet
    /// ones asked for, `quiet` or `merge`.
    pub class: String,
    /// The write the entity shows.
    pub winner: ConflictWriteItem,
    /// The concurrent writes that lost, oldest first.
    pub losers: Vec<ConflictWriteItem>,
    /// For `deleted` when the delete won: the branches that edited the
    /// entity without seeing it.
    pub edited_while_deleted: Vec<ConflictEditItem>,
    /// For `dependency_cycle`: the loop the set-aside edge would close,
    /// from `entity` back to `entity`. The `winner` is then that edge's
    /// own write, the one no read shows (ADR-0065 §5). Empty otherwise.
    #[uniffi(default = [])]
    pub cycle: Vec<EntityRef>,
}

const fn class_name(c: ConflictClass) -> &'static str {
    match c {
        ConflictClass::Quiet => "quiet",
        ConflictClass::Tracked => "tracked",
        ConflictClass::State => "state",
        ConflictClass::Tombstone => "tombstone",
        ConflictClass::Merge => "merge",
        ConflictClass::DependencyCycle => "dependency_cycle",
    }
}

impl From<&ConflictWrite> for ConflictWriteItem {
    fn from(w: &ConflictWrite) -> Self {
        Self {
            version: w.version.to_string(),
            hlc_ms: w.hlc_ms,
            hlc_logical: w.hlc_logical,
            device_id: hex16(&w.device),
            origin: match w.origin {
                WriteOrigin::User => "user",
                WriteOrigin::Generated => "generated",
            }
            .to_owned(),
            value: w
                .value
                .as_ref()
                .and_then(|v| sunrise_cbor::encode_canonical(v).ok()),
        }
    }
}

impl From<&DeletedEdit> for ConflictEditItem {
    fn from(e: &DeletedEdit) -> Self {
        Self {
            version: e.version.to_string(),
            device_id: hex16(&e.device),
            fields: e.fields.clone(),
        }
    }
}

impl From<&EntityConflict> for ConflictItem {
    fn from(c: &EntityConflict) -> Self {
        Self {
            entity: c.entity,
            field: c.field.clone(),
            class: class_name(c.class).to_owned(),
            winner: ConflictWriteItem::from(&c.winner),
            losers: c.losers.iter().map(ConflictWriteItem::from).collect(),
            edited_while_deleted: c
                .edited_while_deleted
                .iter()
                .map(ConflictEditItem::from)
                .collect(),
            cycle: c.cycle.clone(),
        }
    }
}

/// The value bytes a client hands to `CoreCommand::ResolveConflict`, read
/// back into the value the core writes.
///
/// # Errors
/// [`crate::BindingError::Core`] for bytes that are not canonical CBOR.
pub(crate) fn value_in(bytes: &[u8]) -> Result<sunrise_cbor::CborValue, crate::BindingError> {
    sunrise_cbor::decode_canonical(bytes).map_err(|e| {
        crate::BindingError::Core(format!("a conflict value is not canonical CBOR: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::VersionRef;

    #[test]
    fn a_conflict_crosses_with_its_value_as_canonical_cbor() {
        // A one-letter CBOR text, `0x61` and the letter.
        let text = |c: u8| value_in(&[0x61, c]).expect("canonical");
        let write = |seq, letter: u8| ConflictWrite {
            version: VersionRef {
                stream: [1; 16],
                device: [2; 16],
                seq,
            },
            hlc_ms: 7,
            hlc_logical: 1,
            device: [2; 16],
            origin: WriteOrigin::User,
            value: Some(text(letter)),
        };
        let core = EntityConflict {
            entity: EntityRef::new(sunrise_id::EntityKind::Task, [3; 16]),
            field: "title".into(),
            class: ConflictClass::Tracked,
            winner: write(2, b'b'),
            losers: vec![write(1, b'a')],
            edited_while_deleted: Vec::new(),
            cycle: Vec::new(),
        };
        let item = ConflictItem::from(&core);
        assert_eq!(item.class, "tracked");
        assert_eq!(item.winner.origin, "user");
        assert_eq!(item.losers[0].version, core.losers[0].version.to_string());
        let bytes = item.losers[0].value.clone().expect("a value");
        assert_eq!(
            value_in(&bytes).unwrap(),
            core.losers[0].value.clone().unwrap()
        );
        assert!(value_in(&[0xff]).is_err());
    }
}
