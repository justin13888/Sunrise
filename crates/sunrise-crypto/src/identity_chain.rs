//! The account's identity chain, as one record the relay keeps beside the
//! recovery blob ([ADR-0070](../../../docs/11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)).
//!
//! A recovery code opens the identity it was sealed under. After an identity
//! rotation that identity is no longer the account's: the transitions that
//! lead from it to the identity in force travel as `identity_transition` ops
//! in the vault-meta stream, and the relay trims ops. A recovering vault needs
//! those links **before** it can read anything else, because every cert,
//! snapshot record and key bundle it meets verifies only under an identity it
//! can reach. This record is the chain, outside op retention.
//!
//! # Wire shape
//!
//! ```text
//! chain = "SR" 0x09 version(u16 BE = 1) || canonical CBOR map
//! ```
//!
//! | Key | Field | |
//! |---|---|---|
//! | 1 | `genesis_id_s_pub` | `bstr .size 32`, the account's first identity |
//! | 2 | `links` | `[* link]`, genesis first |
//!
//! ```text
//! link = [ to_identity_id, to_id_s_pub, to_id_d_pub, roster_digest,
//!          shares_digest, prev_sig, next_sig, meta_epoch, hlc_physical_ms,
//!          hlc_logical, emitter_device_id, carry_share / null ]
//! ```
//!
//! A link's predecessor is the link before it, or the genesis. Fields one to
//! seven are the transition's signed body and its two signatures, so a link
//! verifies exactly as the op it was cut from does. `carry_share` is the
//! transition's carry-forward share, HPKE-sealed to the **predecessor's**
//! `ID_D_pub`, and `null` where the rotation did not carry the code.
//!
//! # What binds and what does not
//!
//! - **The links.** Each verifies under its predecessor's key and its own
//!   ([`crate::verify_identity_transition`]); a relay holding no `ID_S_priv`
//!   can drop links but cannot add, alter or reorder one.
//! - **The carry shares.** Opened under the predecessor's `ID_D_priv` and
//!   checked against the signed successor keys, so a substituted share fails
//!   to open or opens to keys the body does not name.
//! - **Not the rank.** `meta_epoch`, the HLC and the emitter are the fold's
//!   ordering inputs, carried from the op's envelope, and nothing signs them
//!   here. They matter only between two successors of one predecessor, which
//!   takes that predecessor's `ID_S_priv` to produce, and the op itself
//!   replaces them when it arrives. ADR-0070 §Security.
//!
//! The record carries no roster: rosters name devices, and the relay reads
//! this record. A recovering vault learns the roster from the op or from the
//! vault-meta snapshot record, once the chain lets it verify either.

use crate::hpke_seal::{hpke_open_identity, identity_carry_info};
use crate::identity::identity_id_from_pub;
use crate::identity_transition::{
    verify_identity_transition, IdentityTransitionBody, IdentityTransitionSigs, IDENTITY_SHARE_LEN,
};
use crate::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
use ciborium::value::{Integer, Value};
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MagicPrefix, MAGIC_LEN};
use thiserror::Error;
use zeroize::Zeroize;

/// The identity chain's format version, carried in its magic prefix.
pub const IDENTITY_CHAIN_FORMAT_V: u16 = 1;

/// The most links one record may carry. A rotation is a user action or a
/// revocation, so no account approaches it; it bounds what a hostile record
/// can make a reader verify (two Ed25519 checks a link).
pub const MAX_CHAIN_LINKS: usize = 4096;

/// The largest record a reader decodes: a link is under four hundred bytes.
pub const MAX_CHAIN_BYTES: usize = 2 * 1024 * 1024;

/// One rotation, as the record carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    /// The signed body. `from_identity_id` is the predecessor's id.
    pub body: IdentityTransitionBody,
    /// Its two signatures.
    pub sigs: IdentityTransitionSigs,
    /// The vault-meta epoch the op was sealed under. Unsigned here.
    pub meta_epoch: u32,
    /// The emitter's HLC physical half. Unsigned here.
    pub hlc_physical_ms: u64,
    /// The emitter's HLC logical half. Unsigned here.
    pub hlc_logical: u32,
    /// The device that emitted the op. Unsigned here.
    pub emitter_device_id: [u8; 16],
    /// The carry-forward share, sealed to the predecessor's `ID_D_pub`;
    /// `None` where the rotation retired the code.
    pub carry_share: Option<Vec<u8>>,
}

/// An account's identity chain, genesis first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityChain {
    /// The account's first `ID_S_pub`.
    pub genesis_id_s_pub: [u8; 32],
    /// Every link from the genesis to the identity in force, in order.
    pub links: Vec<ChainLink>,
}

/// Identity chain errors. One variant per thing a caller does differently.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityChainError {
    /// Not an identity chain: the magic prefix, the version, the CBOR, a
    /// field's shape, or a bound.
    #[error("not an identity chain: {0}")]
    Malformed(&'static str),
    /// A well-formed chain of another account.
    #[error("the identity chain starts at another account's identity")]
    OtherAccount,
    /// A link does not verify under its predecessor.
    #[error("link {0} of the identity chain does not verify")]
    BadLink(usize),
}

/// What walking the carry shares from one identity found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryWalk {
    /// The identity is not on the chain at all.
    NotOnChain,
    /// Every link after the identity carried the code forward: the code
    /// reaches the identity in force, `links` rotations later.
    Reaches {
        /// How many links the walk crossed.
        links: usize,
    },
    /// The link at `at_link` retired the code: it carried no share, or one
    /// that does not open under the identity before it.
    Retired {
        /// The index of the link that retired it.
        at_link: usize,
    },
}

fn int(n: u64) -> Value {
    Value::Integer(Integer::from(n))
}

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec())
}

fn prefix() -> [u8; MAGIC_LEN] {
    let mut p = [0u8; MAGIC_LEN];
    write_prefix(&mut p, MagicKind::IdentityChain, IDENTITY_CHAIN_FORMAT_V);
    p
}

impl IdentityChain {
    /// The genesis identity id.
    #[must_use]
    pub fn genesis_identity_id(&self) -> [u8; 16] {
        identity_id_from_pub(&self.genesis_id_s_pub)
    }

    /// The identity the last link reaches: the identity in force, as far as
    /// this record knows.
    #[must_use]
    pub fn head(&self) -> ([u8; 16], [u8; 32]) {
        self.links.last().map_or_else(
            || (self.genesis_identity_id(), self.genesis_id_s_pub),
            |l| (l.body.to_identity_id, l.body.to_id_s_pub),
        )
    }

    /// Encode as `magic ‖ canonical CBOR`.
    ///
    /// # Errors
    /// [`IdentityChainError::Malformed`] past [`MAX_CHAIN_LINKS`].
    pub fn encode(&self) -> Result<Vec<u8>, IdentityChainError> {
        if self.links.len() > MAX_CHAIN_LINKS {
            return Err(IdentityChainError::Malformed("too many links"));
        }
        let links = self
            .links
            .iter()
            .map(|l| {
                Value::Array(vec![
                    bytes(&l.body.to_identity_id),
                    bytes(&l.body.to_id_s_pub),
                    bytes(&l.body.to_id_d_pub),
                    bytes(&l.body.roster_digest),
                    bytes(&l.body.shares_digest),
                    bytes(&l.sigs.prev_sig),
                    bytes(&l.sigs.next_sig),
                    int(u64::from(l.meta_epoch)),
                    int(l.hlc_physical_ms),
                    int(u64::from(l.hlc_logical)),
                    bytes(&l.emitter_device_id),
                    l.carry_share.as_deref().map_or(Value::Null, bytes),
                ])
            })
            .collect();
        let map = Value::Map(vec![
            (int(1), bytes(&self.genesis_id_s_pub)),
            (int(2), Value::Array(links)),
        ]);
        let mut out = prefix().to_vec();
        out.extend_from_slice(
            &sunrise_cbor::encode_canonical(&map)
                .map_err(|_| IdentityChainError::Malformed("encode"))?,
        );
        Ok(out)
    }

    /// Decode a record: the magic prefix, the version, canonical CBOR, and
    /// every field's shape and bound. Nothing is verified yet; see
    /// [`Self::verify`].
    ///
    /// # Errors
    /// [`IdentityChainError::Malformed`].
    pub fn decode(raw: &[u8]) -> Result<Self, IdentityChainError> {
        let bad = IdentityChainError::Malformed;
        if raw.len() > MAX_CHAIN_BYTES {
            return Err(bad("too large"));
        }
        if !matches!(
            decode_prefix(raw),
            Ok(MagicPrefix {
                kind: MagicKind::IdentityChain,
                version: IDENTITY_CHAIN_FORMAT_V,
            })
        ) {
            return Err(bad("magic prefix"));
        }
        let value: Value = sunrise_cbor::decode_canonical(&raw[MAGIC_LEN..])
            .map_err(|_| bad("not canonical CBOR"))?;
        let Value::Map(entries) = value else {
            return Err(bad("not a map"));
        };
        let mut genesis: Option<[u8; 32]> = None;
        let mut links: Option<Vec<ChainLink>> = None;
        for (k, v) in entries {
            match k.as_integer().and_then(|i| u8::try_from(i).ok()) {
                Some(1) if genesis.is_none() => {
                    genesis = Some(fixed(&v).ok_or(bad("genesis_id_s_pub"))?);
                }
                Some(2) if links.is_none() => {
                    let items = v.as_array().ok_or(bad("links"))?;
                    if items.len() > MAX_CHAIN_LINKS {
                        return Err(bad("too many links"));
                    }
                    links = Some(
                        items
                            .iter()
                            .map(|item| decode_link(item).ok_or(bad("link")))
                            .collect::<Result<_, _>>()?,
                    );
                }
                _ => return Err(bad("unknown or duplicate field")),
            }
        }
        let genesis_id_s_pub = genesis.ok_or(bad("missing genesis_id_s_pub"))?;
        let mut links = links.ok_or(bad("missing links"))?;
        // A link's predecessor is positional, so it is filled in here rather
        // than carried: a record cannot name one predecessor and sit after
        // another.
        let mut from = identity_id_from_pub(&genesis_id_s_pub);
        for link in &mut links {
            link.body.from_identity_id = from;
            from = link.body.to_identity_id;
        }
        Ok(Self {
            genesis_id_s_pub,
            links,
        })
    }

    /// Check that the record starts at `genesis_identity_id` and that every
    /// link verifies under its predecessor and its own key.
    ///
    /// # Errors
    /// [`IdentityChainError::OtherAccount`] for another genesis, and
    /// [`IdentityChainError::BadLink`] naming the first link that fails.
    pub fn verify(&self, genesis_identity_id: &[u8; 16]) -> Result<(), IdentityChainError> {
        if &self.genesis_identity_id() != genesis_identity_id {
            return Err(IdentityChainError::OtherAccount);
        }
        let mut from_pub = self.genesis_id_s_pub;
        for (i, link) in self.links.iter().enumerate() {
            verify_identity_transition(&link.body, &from_pub, &link.sigs)
                .map_err(|_| IdentityChainError::BadLink(i))?;
            from_pub = link.body.to_id_s_pub;
        }
        Ok(())
    }

    /// Walk the carry shares forward from `identity_id`, opening each with the
    /// `ID_D_priv` the previous one yielded, starting from `id_d_priv`.
    ///
    /// What a recovery asks before it creates a vault: whether the identity
    /// its code opened still reaches the account, or a later rotation retired
    /// the code. Call [`Self::verify`] first; this checks only that each share
    /// opens to the keys its link names.
    #[must_use]
    pub fn carry_walk(&self, identity_id: &[u8; 16], id_d_priv: &[u8; 32]) -> CarryWalk {
        let start = if &self.genesis_identity_id() == identity_id {
            0
        } else if let Some(i) = self
            .links
            .iter()
            .position(|l| &l.body.to_identity_id == identity_id)
        {
            i + 1
        } else {
            return CarryWalk::NotOnChain;
        };
        let mut dh = IdentityDhKeyPair::from_secret_bytes(*id_d_priv);
        for (i, link) in self.links.iter().enumerate().skip(start) {
            let Some(next) = link
                .carry_share
                .as_deref()
                .and_then(|share| open_carry(&dh, &link.body, share))
            else {
                return CarryWalk::Retired { at_link: i };
            };
            dh = next;
        }
        CarryWalk::Reaches {
            links: self.links.len() - start,
        }
    }
}

/// Open one carry share under `dh` and check it against the link's signed
/// successor keys. The successor's `ID_D` comes back; its `ID_S` is checked
/// and dropped.
fn open_carry(
    dh: &IdentityDhKeyPair,
    body: &IdentityTransitionBody,
    share: &[u8],
) -> Option<IdentityDhKeyPair> {
    if share.len() != IDENTITY_SHARE_LEN {
        return None;
    }
    let mut opened =
        hpke_open_identity(dh, &identity_carry_info(&body.to_identity_id), share, b"").ok()?;
    if opened.len() != 64 {
        opened.zeroize();
        return None;
    }
    let mut s = [0u8; 32];
    let mut d = [0u8; 32];
    s.copy_from_slice(&opened[..32]);
    d.copy_from_slice(&opened[32..]);
    opened.zeroize();
    let signing = IdentitySigningKeyPair::from_secret_bytes(&s);
    s.zeroize();
    let next = IdentityDhKeyPair::from_secret_bytes(d);
    d.zeroize();
    (signing.public_bytes() == body.to_id_s_pub && next.public_bytes() == body.to_id_d_pub)
        .then_some(next)
}

fn fixed<const N: usize>(v: &Value) -> Option<[u8; N]> {
    v.as_bytes()?.as_slice().try_into().ok()
}

fn decode_link(v: &Value) -> Option<ChainLink> {
    let [to_id, to_s, to_d, roster, shares, prev, next, epoch, ms, logical, emitter, carry] =
        v.as_array()?.as_slice()
    else {
        return None;
    };
    let carry_share = match carry {
        Value::Null => None,
        Value::Bytes(b) if b.len() == IDENTITY_SHARE_LEN => Some(b.clone()),
        _ => return None,
    };
    Some(ChainLink {
        body: IdentityTransitionBody {
            from_identity_id: [0u8; 16],
            to_identity_id: fixed(to_id)?,
            to_id_s_pub: fixed(to_s)?,
            to_id_d_pub: fixed(to_d)?,
            roster_digest: fixed(roster)?,
            shares_digest: fixed(shares)?,
        },
        sigs: IdentityTransitionSigs {
            prev_sig: fixed(prev)?,
            next_sig: fixed(next)?,
        },
        meta_epoch: u32::try_from(epoch.as_integer()?).ok()?,
        hlc_physical_ms: u64::try_from(ms.as_integer()?).ok()?,
        hlc_logical: u32::try_from(logical.as_integer()?).ok()?,
        emitter_device_id: fixed(emitter)?,
        carry_share,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hpke_seal::hpke_seal;
    use crate::identity_transition::sign_identity_transition;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    struct Id {
        s: IdentitySigningKeyPair,
        d: IdentityDhKeyPair,
    }

    impl Id {
        fn new(seed: u8) -> Self {
            Self {
                s: IdentitySigningKeyPair::from_secret_bytes(&[seed; 32]),
                d: IdentityDhKeyPair::from_secret_bytes([seed.wrapping_add(100); 32]),
            }
        }
        fn id(&self) -> [u8; 16] {
            identity_id_from_pub(&self.s.public_bytes())
        }
    }

    fn link(from: &Id, to: &Id, carry: bool, rng: &mut ChaCha20Rng) -> ChainLink {
        let body = IdentityTransitionBody {
            from_identity_id: from.id(),
            to_identity_id: to.id(),
            to_id_s_pub: to.s.public_bytes(),
            to_id_d_pub: to.d.public_bytes(),
            roster_digest: [1; 32],
            shares_digest: [2; 32],
        };
        let sigs = sign_identity_transition(&body, &from.s, &to.s).unwrap();
        let carry_share = carry.then(|| {
            let mut plain = [0u8; 64];
            plain[..32].copy_from_slice(&to.s.secret_bytes());
            plain[32..].copy_from_slice(&to.d.secret_bytes());
            hpke_seal(
                &from.d.public_bytes(),
                &identity_carry_info(&to.id()),
                &plain,
                b"",
                rng,
            )
            .unwrap()
        });
        ChainLink {
            body,
            sigs,
            meta_epoch: 2,
            hlc_physical_ms: 1_700_000_000_000,
            hlc_logical: 3,
            emitter_device_id: [9; 16],
            carry_share,
        }
    }

    fn chain(carries: &[bool]) -> (Vec<Id>, IdentityChain) {
        let mut rng = ChaCha20Rng::seed_from_u64(1);
        let ids: Vec<Id> = (0..=carries.len()).map(|i| Id::new(i as u8 + 1)).collect();
        let links = carries
            .iter()
            .enumerate()
            .map(|(i, c)| link(&ids[i], &ids[i + 1], *c, &mut rng))
            .collect();
        let chain = IdentityChain {
            genesis_id_s_pub: ids[0].s.public_bytes(),
            links,
        };
        (ids, chain)
    }

    #[test]
    fn a_chain_round_trips_and_verifies() {
        let (ids, chain) = chain(&[true, false, true]);
        let back = IdentityChain::decode(&chain.encode().unwrap()).unwrap();
        assert_eq!(back, chain);
        back.verify(&ids[0].id()).unwrap();
        assert_eq!(back.head().0, ids[3].id());
        assert_eq!(
            back.verify(&ids[1].id()),
            Err(IdentityChainError::OtherAccount)
        );
    }

    /// A relay holds no identity key, so it can drop a link but cannot swap
    /// one: a link moved under another predecessor verifies under nothing.
    #[test]
    fn a_reordered_or_forged_link_does_not_verify() {
        let (ids, mut chain) = chain(&[true, true]);
        chain.links.swap(0, 1);
        let back = IdentityChain::decode(&chain.encode().unwrap()).unwrap();
        assert_eq!(
            back.verify(&ids[0].id()),
            Err(IdentityChainError::BadLink(0))
        );
    }

    /// The code reaches the head across every link that carried it, and the
    /// first link that did not is where it was retired.
    #[test]
    fn the_carry_walk_reaches_the_head_or_names_the_link_that_retired_the_code() {
        let (ids, chain) = chain(&[true, true]);
        assert_eq!(
            chain.carry_walk(&ids[0].id(), &ids[0].d.secret_bytes()),
            CarryWalk::Reaches { links: 2 }
        );
        assert_eq!(
            chain.carry_walk(&ids[2].id(), &ids[2].d.secret_bytes()),
            CarryWalk::Reaches { links: 0 }
        );
        let (ids, chain) = chain_retired();
        assert_eq!(
            chain.carry_walk(&ids[0].id(), &ids[0].d.secret_bytes()),
            CarryWalk::Retired { at_link: 1 }
        );
        // A code re-issued for the identity after the retiring link is fine.
        assert_eq!(
            chain.carry_walk(&ids[2].id(), &ids[2].d.secret_bytes()),
            CarryWalk::Reaches { links: 1 }
        );
        assert_eq!(
            chain.carry_walk(&[0xee; 16], &[0; 32]),
            CarryWalk::NotOnChain
        );
        // A share sealed to the right key opens only under it.
        assert_eq!(
            chain.carry_walk(&ids[0].id(), &ids[3].d.secret_bytes()),
            CarryWalk::Retired { at_link: 0 }
        );
    }

    fn chain_retired() -> (Vec<Id>, IdentityChain) {
        chain(&[true, false, true])
    }

    #[test]
    fn a_record_with_an_unknown_field_is_refused() {
        let (_, chain) = chain(&[true]);
        let mut raw = chain.encode().unwrap();
        raw[MAGIC_LEN] = 0xa3; // claims a third entry that is not there
        assert!(IdentityChain::decode(&raw).is_err());
        assert!(IdentityChain::decode(b"SR\x08\x00\x01").is_err());
    }
}
