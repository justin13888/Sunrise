//! Forward compatibility: fields this build does not know about, kept anyway.
//!
//! `docs/10-cross-cutting/protocol-versioning.md` §7 states the rule plainly —
//! "**unknown CBOR map keys round-trip unchanged**. A v1 client receiving a v2
//! op preserves the unknown keys verbatim in storage and re-serializes them on
//! outbound merges" — so that "v2-only fields persist on v1-only devices".
//! Until this module existed, nothing implemented it: `serde`'s default
//! behaviour is to DISCARD an unrecognised key, and the entity that came back
//! out of a v1 device had quietly lost every v2 field it arrived with. On an
//! entity-level LWW merge model that is not a cosmetic loss — the v1 device
//! wins one conflict and the v2 fields are gone from every replica.
//!
//! Every entity therefore carries `#[serde(flatten)] unknown: Unknowns`. The
//! one exception is `Interruption`, whose whole value IS its key; see its own
//! doc comment.
//!
//! Two properties make this safe rather than merely well-meant:
//!
//! * **Byte-exact re-emission.** `sunrise_cbor::encode_canonical` sorts map
//!   keys by their encoded bytes, so a preserved field is written back in the
//!   position the sender put it in. Under declaration order it could not be.
//! * **No floats.** `sunrise_cbor::CborValue` refuses them on decode, which is
//!   what makes the `Eq` every entity derives honest.

use std::collections::BTreeMap;

pub use sunrise_cbor::CborValue;

/// Unknown top-level fields of an entity, keyed by their CBOR map key.
pub type Unknowns = BTreeMap<String, CborValue>;

/// An enum value this build does not recognise, held byte for byte.
///
/// The payload of every lossless enum's `Unknown` arm (ADR-0045 §6). It is
/// only ever built from a string that matched none of the enum's known
/// spellings, which is what keeps `TaskState::Unknown(..)` from ever equalling
/// `TaskState::Done`: each enum's `from_raw` is the one constructor that
/// decides between a known arm and this one. [`UnknownVariant::new`] is public
/// because the FFI layer has to lift a value a client sends back; a client
/// that wraps a known spelling in it gets a value that encodes to that
/// spelling and decodes as the known arm on the next read.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnknownVariant(Box<str>);

impl UnknownVariant {
    /// Wrap a raw wire/storage spelling. Prefer the owning enum's `from_raw`,
    /// which routes a known spelling to its known arm.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        Self(raw.into())
    }

    /// The spelling exactly as it arrived.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Make a string-valued enum lossless (ADR-0045 §6).
///
/// A derived `Deserialize` on a unit-only enum REJECTS a variant name it does
/// not know, and rejecting one field's value rejects the whole op it arrived
/// in. Degrading the value to a fallback instead lets the op land, but a
/// degraded value that is *written back* as its fallback overwrites the newer
/// peer's value on every replica the next time this build writes the entity.
///
/// So the enum carries an `Unknown(UnknownVariant)` arm, and this macro gives
/// it the whole string contract from one table:
///
/// * `as_str` — the wire/storage spelling; an unknown value's raw string.
/// * `from_raw` — the inverse, total: a known spelling is its arm, anything
///   else is `Unknown`. `from_raw(x.as_str()) == x` for every value, and
///   `from_raw(s).as_str() == s` for every string.
/// * `Serialize`/`Deserialize` as that string, so an unknown value re-encodes
///   byte for byte and never fails the op it arrived in.
/// * `is_unknown`.
///
/// With `fallback = Arm`, it also generates `FALLBACK` and `effective`, the
/// reading logic uses: an unknown value reads as the named SAFE arm — an
/// unknown task state is `todo`, not `done`; an unknown constraint severity is
/// `soft`, not `hard`. Logic reads the fallback; encoding never writes it.
/// Without a fallback (`Frequency`, `Weekday`) there is no safe arm to read,
/// and the consumer handles `Unknown` itself.
///
/// The name is kept from when it degraded: ADR-0045 and the schema docs name
/// the enums it covers "the `lossy_enum!` users", and it is the input it
/// tolerates, not the output, that is lossy.
macro_rules! lossy_enum {
    ($ty:ident, fallback = $fb:ident, { $($arm:ident => $s:literal),+ $(,)? }) => {
        $crate::unknown::lossy_enum!($ty, { $($arm => $s),+ });

        impl $ty {
            /// The known arm an unknown value reads as.
            pub const FALLBACK: Self = Self::$fb;

            /// The value logic acts on: itself when known, [`Self::FALLBACK`]
            /// when unknown. Never write this back; write `self`.
            #[must_use]
            pub fn effective(&self) -> Self {
                match self {
                    Self::Unknown(_) => Self::FALLBACK,
                    known => known.clone(),
                }
            }
        }
    };
    ($ty:ident, { $($arm:ident => $s:literal),+ $(,)? }) => {
        impl $ty {
            /// Every known arm, in declaration order of the table.
            pub const KNOWN: &'static [Self] = &[$(Self::$arm),+];

            /// The wire/storage spelling. An unknown value's is its raw string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$arm => $s,)+
                    Self::Unknown(raw) => raw.as_str(),
                }
            }

            /// Parse a wire/storage spelling. Total and lossless: anything that
            /// is not a known spelling is kept verbatim as `Unknown`.
            #[must_use]
            pub fn from_raw(s: &str) -> Self {
                match s {
                    $($s => Self::$arm,)+
                    other => Self::Unknown($crate::unknown::UnknownVariant::new(other)),
                }
            }

            /// Whether this value is one this build does not recognise.
            #[must_use]
            pub const fn is_unknown(&self) -> bool {
                matches!(self, Self::Unknown(_))
            }
        }

        impl serde::Serialize for $ty {
            fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D>(d: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = <std::borrow::Cow<'de, str> as serde::Deserialize<'de>>::deserialize(d)?;
                Ok(Self::from_raw(&raw))
            }
        }
    };
}

pub(crate) use lossy_enum;
