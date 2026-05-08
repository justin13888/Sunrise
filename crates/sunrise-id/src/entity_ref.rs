//! Typed entity reference: `<prefix><26-char ULID>`.
//!
//! See `spec/02-domain/identifiers.md` §`EntityRef`.

use crate::{
    crockford::{decode_str, ENCODED_LEN},
    kind::EntityKind,
    ulid::Ulid,
};
use core::fmt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Typed reference to an entity: `(EntityKind, [u8; 16])`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityRef {
    kind: EntityKind,
    bytes: [u8; 16],
}

/// Errors produced when parsing an [`EntityRef`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EntityRefError {
    /// Length wrong; expected 4 (prefix) + 26 (body) = 30.
    #[error("invalid length: expected 30, got {0}")]
    BadLength(usize),
    /// Prefix unknown.
    #[error("unknown entity prefix: {0:?}")]
    UnknownPrefix(String),
    /// Prefix did not match the expected kind.
    #[error("prefix mismatch: expected {expected:?}, got {got:?}")]
    KindMismatch {
        /// Caller's expected kind.
        expected: EntityKind,
        /// Parsed kind.
        got: EntityKind,
    },
    /// Crockford decode of the body failed.
    #[error("body decode error: {0}")]
    BadBody(#[from] crate::crockford::CrockfordError),
}

impl EntityRef {
    /// Construct from a kind and raw bytes.
    #[inline]
    #[must_use]
    pub const fn new(kind: EntityKind, bytes: [u8; 16]) -> Self {
        Self { kind, bytes }
    }

    /// Construct from a kind and ULID.
    #[inline]
    #[must_use]
    pub const fn from_ulid(kind: EntityKind, ulid: Ulid) -> Self {
        Self {
            kind,
            bytes: *ulid.as_bytes(),
        }
    }

    /// Kind discriminator.
    #[inline]
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Raw 16 bytes.
    #[inline]
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    /// As a [`Ulid`] (kind discarded).
    #[inline]
    #[must_use]
    pub const fn ulid(&self) -> Ulid {
        Ulid::from_bytes(self.bytes)
    }

    /// Render as `<prefix><26-char ULID>`.
    #[must_use]
    pub fn to_str(&self) -> String {
        let mut s = String::with_capacity(30);
        s.push_str(self.kind.prefix());
        s.push_str(&Ulid::from_bytes(self.bytes).to_str());
        s
    }

    /// Parse a string. Accepts any kind.
    pub fn parse_any(s: &str) -> Result<Self, EntityRefError> {
        if s.len() != 4 + ENCODED_LEN {
            return Err(EntityRefError::BadLength(s.len()));
        }
        let prefix = &s[..4];
        let kind = EntityKind::from_prefix(prefix)
            .ok_or_else(|| EntityRefError::UnknownPrefix(prefix.to_string()))?;
        let body = &s[4..];
        let bytes = decode_str(body)?;
        Ok(Self { kind, bytes })
    }

    /// Parse a string and assert the kind. Returns an error on prefix mismatch.
    pub fn parse(s: &str, expected: EntityKind) -> Result<Self, EntityRefError> {
        let r = Self::parse_any(s)?;
        if r.kind != expected {
            return Err(EntityRefError::KindMismatch {
                expected,
                got: r.kind,
            });
        }
        Ok(r)
    }
}

impl fmt::Debug for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EntityRef").field(&self.to_str()).finish()
    }
}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_str())
    }
}

impl Serialize for EntityRef {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_str())
    }
}

impl<'de> Deserialize<'de> for EntityRef {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = EntityRef;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 30-char prefixed ULID like `tsk_…`")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EntityRef, E> {
                EntityRef::parse_any(v).map_err(serde::de::Error::custom)
            }
        }
        de.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_str() {
        let bytes = [
            0x01, 0x91, 0x9c, 0x06, 0x39, 0x3e, 0xc5, 0x68, 0x95, 0x6e, 0xb6, 0x95, 0x57, 0xa9,
            0xff, 0x7d,
        ];
        let r = EntityRef::new(EntityKind::Task, bytes);
        let s = r.to_str();
        assert!(s.starts_with("tsk_"));
        assert_eq!(s.len(), 30);
        assert_eq!(EntityRef::parse_any(&s).unwrap(), r);
    }

    #[test]
    fn parse_strict_kind() {
        let r = EntityRef::new(EntityKind::Stream, [0u8; 16]);
        let s = r.to_str();
        assert_eq!(EntityRef::parse(&s, EntityKind::Stream).unwrap(), r);
        assert!(matches!(
            EntityRef::parse(&s, EntityKind::Task),
            Err(EntityRefError::KindMismatch {
                expected: EntityKind::Task,
                got: EntityKind::Stream
            })
        ));
    }

    #[test]
    fn rejects_unknown_prefix() {
        let s = "xxx_00000000000000000000000000";
        assert!(matches!(
            EntityRef::parse_any(s),
            Err(EntityRefError::UnknownPrefix(_))
        ));
    }

    #[test]
    fn rejects_bad_length() {
        assert!(matches!(
            EntityRef::parse_any("tsk_short"),
            Err(EntityRefError::BadLength(_))
        ));
    }

    #[test]
    fn serde_round_trips() {
        let r = EntityRef::new(EntityKind::Task, [0u8; 16]);
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "\"tsk_00000000000000000000000000\"");
        let back: EntityRef = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
