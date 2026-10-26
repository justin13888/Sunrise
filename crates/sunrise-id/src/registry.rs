//! The entity registry: the one declaration of what an entity is.
//!
//! Every entity kind is declared once, in [`crate::for_each_entity!`], with its id
//! prefix, its op-log tag, how it merges, which stream owns its ops, the
//! feature ids scoped to it, the op variants that carry it and the records
//! those ops write: each record's storage projection and every field's wire
//! name, Rust type and CRDT type ([ADR-0044] §2–§3), and each register's
//! conflict class ([ADR-0065] §3).
//!
//! The declaration is an *x-macro*. It holds tokens, not types, and hands them
//! to a callback macro in whichever crate expands it, so each consumer derives
//! its part from the same list:
//!
//! | Consumer | Derives | Fails on a missed entity |
//! |---|---|---|
//! | `sunrise-id` (this module) | [`EntityKind`], its prefixes, [`ENTITIES`] | build |
//! | `sunrise-domain::registry` | a field-by-field check of each record type | build |
//! | `sunrise-core::inner_op` | `InnerOp`'s entity variants and their routing | build |
//! | `sunrise-core::engine::lww` | the materializer's table, key and merge class | build: its per-op match is exhaustive |
//! | `sunrise-core-bindings::dto` | that every synced record names a `UniFFI` mirror that converts from it | build |
//! | `sunrise-storage::db` | that every projected table holds its key and unknowns column | test |
//!
//! [`ENTITIES`] is the machine-readable form: the input the canonical schema
//! and its fingerprint (#323) and per-field merge (#319) are built from.
//!
//! The value types a record carries without being one (`RRule`,
//! `SunriseTime`, …) and the op payloads the registry does not declare are
//! described beside their types with [`describe_value_types!`], which checks
//! each description against its type at build time (#439).
//!
//! [`describe_value_types!`]: crate::describe_value_types
//!
//! Adding an entity is one entry here plus the code no table can write: the
//! domain type, its row writers and their materializer arm, its migration and
//! its `UniFFI` mirror. Each of those is a build or test failure until it exists.
//! `docs/02-domain/overview.md` §Adding an entity walks through it.
//!
//! # Layout
//!
//! The entries are in `registry/table.rs`, the types [`ENTITIES`] is made of
//! in `registry/spec.rs`, [`describe_value_types!`] and the types it fills in
//! `registry/value.rs`, and the expansion into [`EntityKind`] and
//! [`ENTITIES`] in `registry/definition.rs`. Every item is re-exported here,
//! and every macro is exported at the crate root.
//!
//! # Grammar
//!
//! ```text
//! Kind {
//!     prefix: "xyz_",                         // 3 lowercase letters + `_`
//!     tag: "kind",                            // op-log `target_kind`; schema name
//!     merge: Lww | AppendOnly | Control | Unsynced,  // Lww with ops: [] is Patch-only
//!     owner: Meta | Parent | Unowned | Field("stream_id"),
//!     features: ["kind.entity", ...],         // feature ids scoped here (#324)
//!     ops: [                                  // each may carry doc attributes
//!         KindCreate(Payload) = "kind.create", Create, id;
//!     ],                                      // variant(payload) = inner_kind, class, target field
//!     records: [
//!         Record @ ("table", "key", Extra | Body | Plain) | none {
//!             field: Type => Register(Quiet | Tracked | State | Tombstone);
//!             field: Type => Register(Tracked) = "\"json\"";  // its default, see below
//!             field: Type => Map | OrSet | Counter | Nested | Derived;
//!             renamed as "wire_name": Type => Register(Quiet);
//!             ..unknown                       // the flattened `Unknowns` field, if any
//!         }
//!     ],
//! }
//! ```
//!
//! A register names its conflict class and nothing else does: the other CRDT
//! types keep every write, so their class is [`Conflict::Merge`]. A register
//! without a class, a class on anything else, and `Register(Merge)` are build
//! errors.
//!
//! The order of entries is [`EntityKind`]'s declaration order, and the order
//! of `ops` is `InnerOp`'s variant order. Neither is on the wire — CBOR
//! carries names — but both are pinned by tests.
//!
//! # Wire stability
//!
//! The prefix, the tag, every op variant name, its `inner_kind` and every
//! field's wire name are wire or storage contracts. Rename none of them.
//! [ADR-0044] §2 says a name that leaves the registry stays reserved forever,
//! but nothing records or enforces that yet. No name has left, so the grammar
//! has no reserved-name slot and no test checks for reuse. The first change
//! that removes a name adds both.
//!
//! # Defaults
//!
//! [ADR-0044] §2 has the registry declare each field's **default**: the value
//! a field-level create that omits the field reads as (§4). A field whose
//! domain type's decoder supplies one (an `Option`'s `None`, an empty list, a
//! `false` behind `#[serde(default)]`) reads as that, and declares none. A
//! field the decoder requires and the merge's projection supplies declares
//! it after `=`, as the field's value in JSON (`FieldSpec::default`):
//! `Task.title`, `state` and `stream_id`, and `Context.name`. The document
//! schema's fingerprint (#323) hashes each declared default with the rest of
//! its field, so changing one is a `DOC_SCHEMA_V` bump.
//!
//! [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
//! [ADR-0065]: ../../../docs/11-adr/0065-copy-on-write-entity-versions.md
//! [`EntityKind`]: crate::EntityKind

mod definition;
mod spec;
mod table;
mod value;

#[cfg(test)]
mod tests;

pub use definition::{EntityKind, ENTITIES};
pub use spec::{
    Conflict, Crdt, EntitySpec, FieldSpec, Merge, OpClass, OpSpec, Owner, RecordSpec, Storage,
    Unknowns, TERMINAL_STATES,
};
pub use value::{Tagging, ValueField, ValueShape, ValueSpec, VariantShape, VariantSpec};
