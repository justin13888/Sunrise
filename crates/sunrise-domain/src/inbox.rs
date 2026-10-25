//! The private key domain, and how "no stream" is spelled on the wire
//! (ADR-0046 §2, §5).
//!
//! A Task, Block or routine template may belong to no Stream
//! (`stream_id: None`). Content with no stream is sealed under the vault's
//! **private key domain**, a key-hierarchy node that is not a Stream
//! ([`crate::KeyDomain::Private`]): it has no stream row, no stream command
//! names it, and no share carries it.
//!
//! Its 16-byte id is the one the old Inbox sentinel stream used, three zero
//! bytes then ASCII `sunrise.inbox`, so every op an older build sealed into
//! the Inbox is already sealed under it, every replica already holds its
//! epochs, and the relay routes it as before. The ASCII is a historical
//! spelling.
//!
//! # On the wire
//!
//! An absent stream is written as these bytes, the reference every older
//! build reads as its Inbox, and they are read back as `None`, forever
//! ([`wire`]). So an older build shows a stream-less task in its Inbox, and
//! a task it files into its Inbox arrives here with no stream: the two
//! spellings mean the same thing on both sides, and no build has to be locked
//! out of writing tasks for this to hold. The sixteen zero bytes the Inbox
//! used before migration 0017 read as `None` too: no task, block or routine
//! can belong to the vault-meta domain.
//!
//! Nothing selects on these bytes. They are the private domain's id, the
//! wire spelling of no stream, and an input migration 0044 maps to `NULL`.

use sunrise_id::{EntityKind, EntityRef};

/// The private key domain's id: three zero bytes then ASCII `sunrise.inbox`.
///
/// It used to be sixteen zero bytes, which is also the id sunrise-core uses
/// for its vault-meta domain, so stream-less task ops and Stream/routine
/// lifecycle ops shared one key. That was survivable while every key was
/// derived from the vault root and every device held it. It is not
/// survivable under ADR-0024: rotating the meta domain on a revocation would
/// rotate personal content as a side effect, and a meta-scoped grant would
/// carry it.
///
/// The bytes are readable in a hex dump on purpose. The three leading zero
/// bytes keep the value below the smallest timestamp a ULID can carry (bytes
/// 0..6 are a millisecond clock), so no generated Stream id can ever equal
/// it, and they keep the Crockford encoding's first character inside the two
/// bits the padding allows.
pub const PRIVATE_DOMAIN_BYTES: [u8; 16] = [
    0x00, 0x00, 0x00, b's', b'u', b'n', b'r', b'i', b's', b'e', b'.', b'i', b'n', b'b', b'o', b'x',
];

/// The pre-ADR-0046 name of [`PRIVATE_DOMAIN_BYTES`], when the bytes named a
/// sentinel Inbox stream. Kept for the key hierarchy's callers and as the
/// migration input it is.
pub const INBOX_STREAM_BYTES: [u8; 16] = PRIVATE_DOMAIN_BYTES;

/// The vault-meta domain's id, sixteen zero bytes, which the Inbox also used
/// before migration 0017.
pub const META_DOMAIN_BYTES: [u8; 16] = [0u8; 16];

/// The legacy Inbox sentinel as an [`EntityRef`]: what "no stream" is
/// written as, and what a client of the seam that predates optional streams
/// passes for it.
#[must_use]
pub fn inbox_stream_ref() -> EntityRef {
    EntityRef::new(EntityKind::Stream, PRIVATE_DOMAIN_BYTES)
}

/// Display form of [`inbox_stream_ref`].
#[must_use]
pub fn inbox_stream_str() -> String {
    inbox_stream_ref().to_str()
}

/// [`PRIVATE_DOMAIN_BYTES`] in the canonical string form.
pub const INBOX_STREAM_ID: &str = "str_0000076XBEE9MQ6S9ED5Q64VVR";

/// Whether `r` is a spelling of "no stream": the private domain's id, or the
/// vault-meta id the Inbox used before migration 0017.
#[must_use]
pub fn names_no_stream(r: &EntityRef) -> bool {
    r.bytes() == &PRIVATE_DOMAIN_BYTES || r.bytes() == &META_DOMAIN_BYTES
}

/// `r` with every spelling of "no stream" read as `None`.
#[must_use]
pub fn home_stream(r: Option<EntityRef>) -> Option<EntityRef> {
    r.filter(|r| !names_no_stream(r))
}

/// The key a per-stream tally (trends, focus stats, the upcoming no-date
/// lanes) files `stream` under: the stream itself, or [`inbox_stream_ref`]
/// for the no-stream bucket. Stream-less work is a bucket of its own, never
/// left out (ADR-0046 §Consequences).
#[must_use]
pub fn stream_key(stream: Option<EntityRef>) -> EntityRef {
    home_stream(stream).unwrap_or_else(inbox_stream_ref)
}

/// The serde form of an optional `stream_id`: `None` is written as
/// [`inbox_stream_ref`], and that, the pre-0017 zero id, `null` and an
/// absent key all read as `None`. Use with `#[serde(default, with = ...)]`.
pub mod wire {
    use super::{home_stream, inbox_stream_ref};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use sunrise_id::EntityRef;

    /// Write `v`, `None` as the private domain's reference.
    ///
    /// # Errors
    /// The serializer's.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(v: &Option<EntityRef>, s: S) -> Result<S::Ok, S::Error> {
        v.unwrap_or_else(inbox_stream_ref).serialize(s)
    }

    /// Read a stream reference, every spelling of "no stream" as `None`.
    ///
    /// # Errors
    /// The deserializer's.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<EntityRef>, D::Error> {
        Option::<EntityRef>::deserialize(d).map(home_stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[test]
    fn inbox_constants_consistent() {
        assert_eq!(inbox_stream_str(), INBOX_STREAM_ID);
    }

    /// The whole point of the 0017 split: the private id is not the meta id.
    #[test]
    fn the_private_domain_is_not_the_meta_domain() {
        assert_ne!(PRIVATE_DOMAIN_BYTES, META_DOMAIN_BYTES);
    }

    /// A generated ULID's first six bytes are a millisecond timestamp, so any
    /// id minted after 1970 sorts above this one and none can equal it.
    #[test]
    fn no_generated_ulid_can_collide() {
        assert_eq!(&PRIVATE_DOMAIN_BYTES[..3], &[0u8; 3]);
        assert!(PRIVATE_DOMAIN_BYTES[3] != 0, "the bytes must be readable");
    }

    #[test]
    fn the_bytes_are_ascii_sunrise_inbox() {
        assert_eq!(&PRIVATE_DOMAIN_BYTES[3..], b"sunrise.inbox");
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(default, with = "wire")]
        stream_id: Option<EntityRef>,
    }

    /// `None` is written as the sentinel an older build reads as its Inbox,
    /// and every spelling of no stream reads back as `None`.
    #[test]
    fn no_stream_round_trips_as_the_sentinel() {
        let none = serde_json::to_value(Holder { stream_id: None }).unwrap();
        assert_eq!(none, serde_json::json!({ "stream_id": INBOX_STREAM_ID }));
        let real = EntityRef::new(EntityKind::Stream, [7; 16]);
        for (json, want) in [
            (serde_json::json!({ "stream_id": INBOX_STREAM_ID }), None),
            (
                serde_json::json!({
                    "stream_id": EntityRef::new(EntityKind::Stream, META_DOMAIN_BYTES).to_str()
                }),
                None,
            ),
            (serde_json::json!({ "stream_id": null }), None),
            (serde_json::json!({}), None),
            (
                serde_json::json!({ "stream_id": real.to_str() }),
                Some(real),
            ),
        ] {
            let got: Holder = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(got.stream_id, want, "{json}");
        }
        let back = serde_json::to_value(Holder {
            stream_id: Some(real),
        })
        .unwrap();
        assert_eq!(back, serde_json::json!({ "stream_id": real.to_str() }));
    }
}
