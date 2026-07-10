//! Loro-backed CRDT layer for Sunrise streams.
//!
//! Implements `docs/05-sync/crdt-design.md` on top of `loro`. Each Stream
//! gets one [`StreamDoc`] (a wrapper around `loro::LoroDoc`); the vault-meta
//! log gets a separate doc.
//!
//! # Field-type mapping
//!
//! | Domain field            | Loro container       |
//! |-------------------------|----------------------|
//! | scalar (title, state)   | `LoroMap` LWW value  |
//! | `contexts` (Set)        | `LoroMap` w/ keyed presence (OR-Set) |
//! | `body` (NoteBody)       | `LoroText` (RichText)|
//! | `stream_order`          | `LoroMovableList`    |
//! | `streak_counter`        | `LoroCounter`        |
//! | `deferred_count`        | `LoroCounter`        |
//!
//! # Convergence
//!
//! Loro guarantees that any permutation of the same set of ops applied to N
//! replicas converges to identical state. Our [`StreamDoc::export_updates`]
//! and [`StreamDoc::import_updates`] route binary updates between replicas;
//! the convergence property test in `tests/convergence.rs` exercises this
//! across random op streams.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod doc;
pub mod merge_journal;

pub use doc::{StreamDoc, StreamDocError};
pub use merge_journal::{MergeEvent, MergeJournal};
