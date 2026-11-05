use super::*;
use std::collections::BTreeSet;

#[test]
fn entries_are_in_entity_kind_order() {
    assert_eq!(ENTITIES.len(), EntityKind::COUNT);
    for (spec, kind) in ENTITIES.iter().zip(EntityKind::all()) {
        assert_eq!(spec.kind, kind);
        assert!(std::ptr::eq(kind.spec(), spec));
        assert_eq!(spec.prefix, kind.prefix());
        assert_eq!(spec.tag, kind.tag());
    }
}

#[test]
fn names_are_unique_where_the_wire_needs_them_to_be() {
    let unique = |names: Vec<&str>, what: &str| {
        let set: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(set.len(), names.len(), "duplicate {what}: {names:?}");
    };
    unique(ENTITIES.iter().map(|e| e.tag).collect(), "tag");
    let ops = || ENTITIES.iter().flat_map(|e| e.ops.iter());
    unique(ops().map(|o| o.variant).collect(), "op variant");
    unique(ops().map(|o| o.inner_kind).collect(), "inner_kind");
    for record in ENTITIES.iter().flat_map(|e| e.records.iter()) {
        unique(record.fields.iter().map(|f| f.name).collect(), "field name");
        unique(record.fields.iter().map(|f| f.rust).collect(), "rust field");
    }
}

/// The merge class decides what the rest of an entry must hold, and a
/// half-declared entity is exactly what this registry exists to stop.
#[test]
fn every_entry_is_complete_for_its_merge_class() {
    for e in &ENTITIES {
        match e.merge {
            Merge::Lww | Merge::AppendOnly => {
                // An `Lww` entity with no op family of its own is written
                // only by `Patch` (Preferences); an append-only one has
                // nothing else to be written by.
                assert!(
                    !e.ops.is_empty() || e.merge == Merge::Lww,
                    "{:?} syncs but has no op",
                    e.kind
                );
                assert!(
                    e.storage().is_some(),
                    "{:?} syncs but its first record is not projected",
                    e.kind
                );
                if let Some(first) = e.ops.first() {
                    assert_eq!(
                        first.class,
                        OpClass::Create,
                        "{:?}'s first op must create it",
                        e.kind
                    );
                }
                assert_ne!(e.owner, Owner::Unowned, "{:?} has ops", e.kind);
            }
            Merge::Control | Merge::Unsynced => {
                assert!(e.ops.is_empty(), "{:?} has no entity ops", e.kind);
            }
        }
        if e.merge == Merge::AppendOnly {
            assert!(
                e.ops.iter().all(|o| o.class != OpClass::Delete),
                "{:?} is append-only and cannot be deleted",
                e.kind
            );
        }
        if e.merge == Merge::Unsynced {
            assert_eq!(e.owner, Owner::Unowned);
        }
    }
}

/// ADR-0065 §3: every register has a class that picks one winner, and
/// nothing else has one. The two special classes sit on the one field each
/// is defined for: `State` on `Task.state`, `Tombstone` on the `deleted`
/// of every entity that syncs by field.
#[test]
fn every_register_declares_a_conflict_class() {
    for e in &ENTITIES {
        for (i, r) in e.records.iter().enumerate() {
            for f in r.fields {
                let at = format!("{}.{}", r.name, f.name);
                if f.crdt == Crdt::Register {
                    assert_ne!(f.conflict, Conflict::Merge, "{at} is a register");
                } else {
                    assert_eq!(f.conflict, Conflict::Merge, "{at} is not a register");
                }
                assert_eq!(
                    f.conflict == Conflict::State,
                    e.kind == EntityKind::Task && f.name == "state",
                    "{at}: only Task.state is State"
                );
                let own_tombstone = e.merge == Merge::Lww && i == 0 && f.name == "deleted";
                assert_eq!(
                    f.conflict == Conflict::Tombstone,
                    own_tombstone,
                    "{at}: Tombstone is the deleted flag of a field-merged entity"
                );
            }
        }
    }
    assert_eq!(TERMINAL_STATES, ["done", "cancelled"]);
}

#[test]
fn op_kinds_and_targets_name_their_entity() {
    for e in &ENTITIES {
        for op in e.ops {
            assert!(
                op.variant.starts_with(match e.kind {
                    EntityKind::FocusSession => "Focus",
                    _ => e.records[0].name,
                }),
                "{} does not name {:?}",
                op.variant,
                e.kind
            );
            let family = op.inner_kind.split('.').next().unwrap_or_default();
            assert!(
                e.tag.starts_with(family),
                "{} is not in {}'s family",
                op.inner_kind,
                e.tag
            );
            assert!(
                e.records
                    .iter()
                    .any(|r| r.fields.iter().any(|f| f.rust == op.target)),
                "{}'s target `{}` is not a registered field",
                op.variant,
                op.target
            );
        }
    }
}

#[test]
fn owner_fields_and_storage_agree_with_the_records() {
    for e in &ENTITIES {
        match e.owner {
            Owner::Field(name) => assert!(
                e.records[0].fields.iter().any(|f| f.rust == name),
                "{:?}'s owner field `{name}` is not on its first record",
                e.kind
            ),
            Owner::Parent => assert!(
                e.records[0].fields.iter().any(|f| f.rust == "parent"),
                "{:?} is owned by a parent it does not name",
                e.kind
            ),
            Owner::Meta | Owner::Unowned => {}
        }
        for r in e.records {
            if let Some(s) = r.storage {
                assert_eq!(
                    s.unknowns != Unknowns::Plain,
                    r.unknowns.is_some(),
                    "{}: a record with an unknowns map needs a column for it, and only then",
                    r.name
                );
                assert!(
                    r.fields
                        .iter()
                        .any(|f| f.rust == "id" || f.rust == "session_id"),
                    "{} is keyed on a field it does not declare",
                    r.name
                );
            }
            for f in r.fields {
                if f.crdt == Crdt::Nested {
                    assert!(
                        e.records.iter().any(|n| n.name == f.value_type),
                        "{}.{} nests an unregistered record",
                        r.name,
                        f.rust
                    );
                }
            }
        }
    }
}
