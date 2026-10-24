//! How a [`SunriseTime`] is written down: its wire form, and the `_tz`
//! sidecar that holds a kind this build does not know in storage.
//!
//! Kept apart from the value's semantics in the parent module because the two
//! change for different reasons: this changes when an encoding does, and the
//! parent when what a time means does.

use super::{kind, SunriseTime};
use crate::unknown::Unknowns;
use jiff::civil;
use jiff::Timestamp;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

/// The prefix of the `_tz` sidecar that holds an [`SunriseTime::Unknown`]
/// value's other fields: this, then their canonical CBOR in lowercase hex.
const RAW_SIDECAR_PREFIX: &str = "cbor:";

/// The tagged wire form, and the pre-`SunriseTime` form it replaced.
///
/// `Deserialize` is hand-rolled rather than derived so a payload written at
/// `DOC_SCHEMA_V = 1` — where these fields were a bare RFC 3339 instant —
/// still reads, as an [`SunriseTime::Instant`]. That is the promise
/// `docs/10-cross-cutting/protocol-versioning.md` makes about minor schema
/// changes, and the cheapest possible place to keep it.
///
/// The third form is a kind this build does not know. It is tried last, so a
/// value of a known kind always decodes as that kind; a known kind whose
/// fields do not parse is refused rather than kept as unknown, because a
/// build that knows the kind would read the same bytes as an error too.
///
/// `untagged` buffers the value once and each form reads that buffer, so the
/// cost of a peer's value is linear in its size.
#[derive(Deserialize)]
#[serde(untagged)]
enum SunriseTimeRepr {
    Tagged(Tagged),
    /// `DOC_SCHEMA_V = 1`: a bare instant, no kind.
    Legacy(Timestamp),
    /// A kind this build does not know.
    Unknown(UnknownTagged),
}

/// The four known kinds. Closed shapes: a key one of them does not know is
/// ignored and not re-emitted, the one recorded exception to lossless unknowns
/// (ADR-0045 §6). A newer build that needs another field adds a kind instead.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Tagged {
    Instant { at: Timestamp },
    Zoned { civil: civil::DateTime, tz: String },
    Floating { civil: civil::DateTime },
    AllDay { date: civil::Date },
}

#[derive(Deserialize)]
struct UnknownTagged {
    kind: String,
    #[serde(flatten)]
    raw: Unknowns,
}

/// The wire form of the four known kinds, borrowed. Derived, so the known
/// kinds encode exactly as they did before `Unknown` existed.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedRef<'a> {
    Instant {
        at: &'a Timestamp,
    },
    Zoned {
        civil: &'a civil::DateTime,
        tz: &'a str,
    },
    Floating {
        civil: &'a civil::DateTime,
    },
    AllDay {
        date: &'a civil::Date,
    },
}

impl Serialize for SunriseTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Instant { at } => TaggedRef::Instant { at }.serialize(s),
            Self::Zoned { civil, tz } => TaggedRef::Zoned { civil, tz }.serialize(s),
            Self::Floating { civil } => TaggedRef::Floating { civil }.serialize(s),
            Self::AllDay { date } => TaggedRef::AllDay { date }.serialize(s),
            Self::Unknown { kind, raw } => {
                // The decoder never puts `kind` in `raw`; a value built by hand
                // could, and a map must not carry the key twice.
                let fields = raw.iter().filter(|(k, _)| k.as_str() != "kind");
                let mut m = s.serialize_map(Some(1 + fields.clone().count()))?;
                m.serialize_entry("kind", kind)?;
                for (k, v) in fields {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SunriseTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match SunriseTimeRepr::deserialize(d)? {
            SunriseTimeRepr::Tagged(Tagged::Instant { at }) | SunriseTimeRepr::Legacy(at) => {
                Self::Instant { at }
            }
            SunriseTimeRepr::Tagged(Tagged::Zoned { civil, tz }) => Self::Zoned { civil, tz },
            SunriseTimeRepr::Tagged(Tagged::Floating { civil }) => Self::Floating { civil },
            SunriseTimeRepr::Tagged(Tagged::AllDay { date }) => Self::AllDay { date },
            SunriseTimeRepr::Unknown(UnknownTagged { kind, raw }) => {
                if kind::KNOWN.contains(&kind.as_str()) {
                    return Err(serde::de::Error::custom(format!("malformed `{kind}` time")));
                }
                Self::Unknown { kind, raw }
            }
        })
    }
}

/// The wire form of a field that was a bare instant before `DOC_SCHEMA_V` 15
/// and is a [`SunriseTime`] since: `Routine.starts_at`, `ends_at`,
/// `paused_until` and `last_completed_at`, `RRule.until` and
/// `Stream.paused_until` (#506). Use with `#[serde(with = "…compat")]`, or
/// `compat::option` for an `Option`.
///
/// An [`SunriseTime::Instant`] is written as the bare timestamp the field
/// always held, so a value that means what it meant before reads, on a build
/// that predates the change, exactly as it always did. Every other kind is
/// written in the tagged form of every `SunriseTime`, which such a build
/// cannot read: an op carrying one uses the feature `routine.civil_times`
/// and is written only into a vault that requires it
/// (`crates/sunrise-core/src/feature.rs`). Reading accepts both forms, as
/// every `SunriseTime` does.
///
/// In the canonical document schema the field's type is `CompatTime`, an
/// alias of `SunriseTime` (`crate::registry`), so the encoding is part of the
/// fingerprint.
pub mod compat {
    use super::SunriseTime;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Write `t`, an instant as a bare timestamp.
    ///
    /// # Errors
    /// The serializer's.
    pub fn serialize<S: Serializer>(t: &SunriseTime, s: S) -> Result<S::Ok, S::Error> {
        match t {
            SunriseTime::Instant { at } => at.serialize(s),
            other => other.serialize(s),
        }
    }

    /// Read either form.
    ///
    /// # Errors
    /// The deserializer's, or a value that is no `SunriseTime`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SunriseTime, D::Error> {
        SunriseTime::deserialize(d)
    }

    /// The same, for an optional field.
    pub mod option {
        use super::SunriseTime;
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        struct Compat<'a>(&'a SunriseTime);

        impl Serialize for Compat<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                super::serialize(self.0, s)
            }
        }

        /// Write `t`, an instant as a bare timestamp.
        ///
        /// # Errors
        /// The serializer's.
        // serde hands a `with` module a reference to the field.
        #[allow(clippy::ref_option)]
        pub fn serialize<S: Serializer>(t: &Option<SunriseTime>, s: S) -> Result<S::Ok, S::Error> {
            match t {
                Some(t) => s.serialize_some(&Compat(t)),
                None => s.serialize_none(),
            }
        }

        /// Read either form, or null.
        ///
        /// # Errors
        /// The deserializer's, or a value that is no `SunriseTime`.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<SunriseTime>, D::Error> {
            Option::<SunriseTime>::deserialize(d)
        }
    }
}

/// The `_tz` sidecar of an [`SunriseTime::Unknown`] value: its other fields'
/// canonical CBOR, hex-encoded after [`RAW_SIDECAR_PREFIX`].
pub(super) fn encode_raw_sidecar(raw: &Unknowns) -> String {
    // A map of `CborValue`s holds no float and nothing else the canonical
    // encoder refuses, so this does not fail; an empty body is the fallback
    // a failure would read back as.
    let bytes = sunrise_cbor::encode_canonical(raw).unwrap_or_default();
    let mut s = String::with_capacity(RAW_SIDECAR_PREFIX.len() + bytes.len() * 2);
    s.push_str(RAW_SIDECAR_PREFIX);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The inverse of [`encode_raw_sidecar`]; `None` for a sidecar it did not
/// write.
pub(super) fn decode_raw_sidecar(s: &str) -> Option<Unknowns> {
    let hex = s.strip_prefix(RAW_SIDECAR_PREFIX)?.as_bytes();
    if hex.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let bytes = hex
        .chunks_exact(2)
        .map(|p| Some((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect::<Option<Vec<u8>>>()?;
    if bytes.is_empty() {
        return Some(Unknowns::new());
    }
    sunrise_cbor::decode_lenient(&bytes).ok()
}
