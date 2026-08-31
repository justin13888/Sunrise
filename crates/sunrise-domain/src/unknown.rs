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

/// Give an enum a `Deserialize` that never fails on an unfamiliar variant.
///
/// A derived `Deserialize` on a unit-only enum REJECTS a variant name it does
/// not know, and rejecting one field's value rejects the whole op it arrived
/// in. On an entity-level merge model that means a newer peer's Task does not
/// merely lose its new `state` — it does not land at all, and the two replicas
/// diverge permanently over one string.
///
/// So an unknown variant degrades to a named fallback, exactly as
/// `StreamColor::from_str_lossy` has always done. The fallback is chosen so the
/// degraded reading is the SAFE one: an unknown task state is `todo`, not
/// `done`; an unknown constraint severity is `soft`, not `hard`. Losing
/// fidelity is acceptable, inventing a completion or a hard block is not.
///
/// `Serialize` stays derived, so a value this build DID understand is written
/// back exactly as it read it. A degraded value is written back as its
/// fallback — the alternative would be to carry the original string, which
/// belongs in an entity-level `unknown` map rather than in the enum.
macro_rules! lossy_enum {
    ($ty:ty) => {
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D>(d: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = <std::borrow::Cow<'de, str> as serde::Deserialize<'de>>::deserialize(d)?;
                Ok(Self::from_str_lossy(&raw))
            }
        }
    };
}

pub(crate) use lossy_enum;
