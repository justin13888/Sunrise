//! The `NoteBody` block grammar: decode, encode, and export to Markdown.
//!
//! Implements `docs/02-domain/notes.md` §Body grammar. Read the distinction
//! that file opens with, because the whole shape of this module follows from
//! it:
//!
//! > `NoteBody` is an **opaque byte string** on the wire. The core neither
//! > parses nor validates its contents; the grammar below is the contract
//! > editors and renderers agree on *inside* those bytes, not a shape the
//! > CBOR codec enforces.
//!
//! So this module is a **renderer contract**, not a validator. Two
//! consequences it is built around:
//!
//! 1. **Decoding cannot fail.** [`decode`](decode()) is total. A body that is
//!    not CBOR, or is CBOR the grammar does not name, still produces the best
//!    rendering this build can manage — down to plain UTF-8 lines as
//!    paragraphs, which is exactly what the `sunrise` CLI writes ("plain text
//!    only; no structured-body editing"). The failure mode is "render as best
//!    you can", never "drop the bytes".
//! 2. **Adding a block kind is not a `DOC_SCHEMA_V` change**, so an older
//!    build meeting a newer body must not corrupt it. That is what
//!    [`Fidelity`] is for.
//!
//! # Fidelity is the safety interlock
//!
//! [`decode`](decode()) reports [`Fidelity::Exact`] only when re-encoding the
//! blocks it produced reproduces the input bytes *exactly*. Anything else — an
//! unknown block kind, an unknown mark, a key this build does not model, a
//! body that was never CBOR — is [`Fidelity::Lossy`].
//!
//! That single rule is unfoolable, because it does not enumerate the ways a
//! body can be strange; it just checks whether this build can put it back.
//! A structured editor **must not save over a `Lossy` body**: doing so would
//! silently drop whatever it could not name. It should render what it got,
//! and leave the original bytes alone.
//!
//! The rule has exactly one documented exception: an **absent** body — no
//! bytes at all — is the same document as an **empty** one (`[]`, a single
//! `0x80`), so it is [`Fidelity::Exact`] even though re-encoding picks the
//! latter spelling. Both mean "no content", and which of the two a field
//! holds is `clear_body`'s business, not the codec's.
//!
//! # Merge
//!
//! A `NoteBody` is one last-writer-wins unit on `(hlc, device_id, seq)`
//! (ADR-0014). Two devices editing one body produce **one survivor, not a
//! character-level merge**; this workspace ships no text CRDT. Nothing in
//! this module changes that, and no editor built on it should imply
//! otherwise.

mod decode;
mod encode;
mod grammar;
mod markdown;

pub use decode::decode;
pub use encode::encode;
pub use grammar::{
    ChecklistItem, Fidelity, HeadingLevel, Inline, ListItem, Mark, NoteBlock, NoteDoc,
    MAX_NEST_DEPTH, NOTE_BODY_MAX_BYTES, NOTE_BODY_SOFT_LIMIT_BYTES,
};
pub use markdown::to_markdown;
