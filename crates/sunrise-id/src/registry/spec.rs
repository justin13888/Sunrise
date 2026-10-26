//! The spec types: what [`super::ENTITIES`] holds for each entity kind, its
//! ops, its records and their fields.

use super::EntityKind;

/// How an entity's ops merge into its materialized state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Every field merges by its own CRDT type (ADR-0044): a full-state op is
    /// a write to every field it carries, and a `Patch` writes the fields it
    /// names.
    Lww,
    /// Each op writes one immutable record under its own key and never
    /// contends with another (ADR-0013).
    AppendOnly,
    /// Carried only by control ops, which hold key material or trust and
    /// never reach the entity materializer (ADR-0024, ADR-0032).
    Control,
    /// No op family carries it yet: the type exists, nothing syncs it.
    Unsynced,
}

/// Which stream's key seals an entity's ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The vault-meta stream.
    Meta,
    /// The stream the named field of the entity's first record holds.
    Field(&'static str),
    /// The stream of the entity its `parent` field names.
    Parent,
    /// No op is ever sealed for it ([`Merge::Unsynced`]).
    Unowned,
}

/// An op's effect on its entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// Brings the entity into being.
    Create,
    /// Changes it, or adds a record to it.
    Update,
    /// Tombstones it.
    Delete,
}

/// How a field merges ([ADR-0044] §3).
///
/// [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crdt {
    /// One last-writer-wins value: scalars, optionals, and nested values
    /// written as a unit.
    Register,
    /// One register per key.
    Map,
    /// Add-wins observed-remove set.
    OrSet,
    /// PN-counter.
    Counter,
    /// A nested record whose own fields each merge by their declared type.
    Nested,
    /// Computed at read time from other fields; not a field of the merge.
    Derived,
}

/// What a field's concurrent writes come to ([ADR-0065] §3).
///
/// Ordered writes are the same for every class: the later one wins. The class
/// decides only between writes neither of which had seen the other. It is part
/// of the schema fingerprint, so changing one is a `DOC_SCHEMA_V` bump.
///
/// [ADR-0065]: ../../../docs/11-adr/0065-copy-on-write-entity-versions.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// Every write survives, so there is no conflict: OR-sets, counters and
    /// maps, and the nested and derived fields no write names.
    Merge,
    /// A register decided by stamp. The loser is kept and not surfaced.
    Quiet,
    /// A register decided by stamp. The loser is kept and surfaced as a
    /// conflict on the entity.
    Tracked,
    /// `Task.state`: a state in [`TERMINAL_STATES`] beats any other, and two
    /// terminal states are decided by stamp. The loser is kept and surfaced.
    State,
    /// `deleted`: a delete beats a concurrent restore, and a concurrent write
    /// to another field never resurrects the entity.
    Tombstone,
}

/// The `Task.state` spellings [`Conflict::State`] ranks above every other
/// state, known or not ([ADR-0065] §3).
///
/// It is part of the schema fingerprint: two builds that disagreed about it
/// would pick different winners from the same versions.
///
/// [ADR-0065]: ../../../docs/11-adr/0065-copy-on-write-entity-versions.md
pub const TERMINAL_STATES: &[&str] = &["done", "cancelled"];

/// Where a record keeps the fields this build does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknowns {
    /// In an `extra` CBOR blob column beside the projected columns.
    Extra,
    /// Inside the row's whole-record CBOR `body` column.
    Body,
    /// Nowhere: the record has no unknowns map.
    Plain,
}

/// One registered entity kind.
#[derive(Debug, Clone, Copy)]
pub struct EntitySpec {
    /// The kind.
    pub kind: EntityKind,
    /// The id prefix.
    pub prefix: &'static str,
    /// The op-log `target_kind` and the entity's schema name.
    pub tag: &'static str,
    /// How its ops merge.
    pub merge: Merge,
    /// Which stream seals its ops.
    pub owner: Owner,
    /// The feature ids scoped to it (ADR-0045 §7).
    pub features: &'static [&'static str],
    /// The op variants that carry it, in `InnerOp` order.
    pub ops: &'static [OpSpec],
    /// The records its ops write. The first is the entity's own.
    pub records: &'static [RecordSpec],
}

/// One op variant.
#[derive(Debug, Clone, Copy)]
pub struct OpSpec {
    /// The `InnerOp` variant name, which is the CBOR tag on the wire.
    pub variant: &'static str,
    /// The payload type, as written in the registry.
    pub payload: &'static str,
    /// The op-log `inner_kind`.
    pub inner_kind: &'static str,
    /// The op's effect.
    pub class: OpClass,
    /// The payload field naming the target entity.
    pub target: &'static str,
}

/// One record type an entity's ops write.
#[derive(Debug, Clone, Copy)]
pub struct RecordSpec {
    /// The Rust type name in `sunrise-domain`.
    pub name: &'static str,
    /// Its materialized table, or `None` when it is not projected on its own.
    pub storage: Option<Storage>,
    /// Its fields, in declaration order.
    pub fields: &'static [FieldSpec],
    /// The Rust name of its flattened unknowns field, if it has one.
    pub unknowns: Option<&'static str>,
}

/// A record's storage projection.
#[derive(Debug, Clone, Copy)]
pub struct Storage {
    /// The table.
    pub table: &'static str,
    /// The column holding the entity id.
    pub key: &'static str,
    /// Where the row keeps unknown fields.
    pub unknowns: Unknowns,
}

/// One field of a record.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// The CBOR map key, which is the field's identity ([ADR-0044] §2).
    ///
    /// [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
    pub name: &'static str,
    /// The Rust field name.
    pub rust: &'static str,
    /// The value type, as written in the registry.
    pub value_type: &'static str,
    /// How it merges.
    pub crdt: Crdt,
    /// What its concurrent writes come to: declared on a register,
    /// [`Conflict::Merge`] on anything else.
    pub conflict: Conflict,
    /// The value it reads as when no op has written it, as JSON, for a field
    /// the record's decoder requires; `None` for a field whose decoder
    /// supplies its own default. See the [module docs](crate::registry)
    /// §Defaults.
    pub default: Option<&'static str>,
}

impl EntitySpec {
    /// The materialized table the entity's merge stamp lives in: its first
    /// record's.
    #[must_use]
    pub fn storage(&self) -> Option<Storage> {
        self.records.first().and_then(|r| r.storage)
    }

    /// The registered record named `name`.
    #[must_use]
    pub fn record(&self, name: &str) -> Option<&'static RecordSpec> {
        self.records.iter().find(|r| r.name == name)
    }
}
