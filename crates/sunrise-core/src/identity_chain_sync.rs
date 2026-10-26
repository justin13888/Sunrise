//! The account's identity chain, carried between a device and the relay
//! ([ADR-0070](../../../docs/11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)).
//!
//! One leg, driven by the sync driver beside the key bundle's: before a
//! session reports `Live`, and on the resync tick, a device that signs under
//! the identity in force compares the chain it folds with the record the
//! relay pins, and pins its own when the relay's is absent, is not this
//! account's, does not verify, or stops short of it. A record that already
//! reaches past this device's is left alone: that device is behind, not the
//! relay.
//!
//! The other leg needs no transport of the driver's. A recovery reads the
//! record beside the recovery blob, on the route the step-up opens, before
//! there is a vault to sync (`sunrise_client_core::recovery`).

use sunrise_crypto::IdentityChain;
use sunrise_sync::{IdentityChainPinned, Transport, TransportError};

use crate::core::Core;
use crate::engine::hex_short;

/// Whether the record the relay pins already reaches at least as far as
/// `mine` along the same links. Anything else is replaced.
fn pinned_covers(mine: &IdentityChain, pinned: &[u8]) -> bool {
    let Ok(theirs) = IdentityChain::decode(pinned) else {
        return false;
    };
    if theirs.verify(&mine.genesis_identity_id()).is_err() || theirs.links.len() < mine.links.len()
    {
        return false;
    }
    mine.links
        .iter()
        .zip(&theirs.links)
        .all(|(a, b)| a.body == b.body && a.sigs == b.sigs)
}

/// Publish this device's identity chain record if the relay's is behind it.
///
/// Failures are logged and left for the next tick, which asks again.
pub(crate) async fn publish<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(Some(mine)) = core.identity_chain_plan() else {
        return;
    };
    let Ok(chain) = IdentityChain::decode(&mine) else {
        return;
    };
    let digest = *blake3::hash(&mine).as_bytes();
    let replaces = match transport.identity_chain_latest().await {
        Ok(None) => None,
        Ok(Some(pinned)) if pinned == mine => {
            core.note_identity_chain_current(digest);
            return;
        }
        Ok(Some(pinned)) if pinned_covers(&chain, &pinned) => return,
        Ok(Some(pinned)) => Some(*blake3::hash(&pinned).as_bytes()),
        Err(TransportError::Unsupported) => return,
        Err(e) => {
            tracing::warn!(
                ev = "sync.identity_chain.publish_failed",
                err_code = "SYNC_NETWORK_UNAVAILABLE",
                err_kind = "transient",
                retryable = true,
                cause = %e,
                "the account's identity chain did not reach the relay; the next tick tries again"
            );
            return;
        }
    };
    match transport.identity_chain_pin(&mine, replaces.as_ref()).await {
        Ok(IdentityChainPinned::Pinned) => {
            core.note_identity_chain_current(digest);
            let (head, _) = chain.head();
            tracing::info!(
                ev = "sync.identity_chain.published",
                head_h = hex_short(&head),
                n_total = chain.links.len() as u64,
                "the account's identity chain is pinned on the relay for a recovery from an older code"
            );
        }
        // Another device pinned first; the next tick judges its record.
        Ok(IdentityChainPinned::Superseded) | Err(TransportError::Unsupported) => {}
        Err(e) => tracing::warn!(
            ev = "sync.identity_chain.publish_failed",
            err_code = "SYNC_NETWORK_UNAVAILABLE",
            err_kind = "transient",
            retryable = true,
            cause = %e,
            "the account's identity chain did not reach the relay; the next tick tries again"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::{
        identity_id_from_pub, sign_identity_transition, ChainLink, IdentityDhKeyPair,
        IdentitySigningKeyPair, IdentityTransitionBody,
    };

    fn signer(seed: u8) -> IdentitySigningKeyPair {
        IdentitySigningKeyPair::from_secret_bytes(&[seed; 32])
    }

    /// A link from the identity seeded `from` to the one seeded `to`, signed
    /// by both, as a rotation writes it.
    fn link(from: u8, to: u8) -> ChainLink {
        let (from_s, to_s) = (signer(from), signer(to));
        let body = IdentityTransitionBody {
            from_identity_id: identity_id_from_pub(&from_s.public_bytes()),
            to_identity_id: identity_id_from_pub(&to_s.public_bytes()),
            to_id_s_pub: to_s.public_bytes(),
            to_id_d_pub: IdentityDhKeyPair::from_secret_bytes([to; 32]).public_bytes(),
            roster_digest: [1; 32],
            shares_digest: [2; 32],
        };
        let sigs = sign_identity_transition(&body, &from_s, &to_s).unwrap();
        ChainLink {
            body,
            sigs,
            meta_epoch: 2,
            hlc_physical_ms: 1_700_000_000_000,
            hlc_logical: 0,
            emitter_device_id: [9; 16],
            carry_share: None,
        }
    }

    /// The chain whose genesis is seeded `path[0]` and which rotates through
    /// each later seed in turn.
    fn chain(path: &[u8]) -> IdentityChain {
        IdentityChain {
            genesis_id_s_pub: signer(path[0]).public_bytes(),
            links: path.windows(2).map(|w| link(w[0], w[1])).collect(),
        }
    }

    fn covers(mine: &[u8], pinned: &IdentityChain) -> bool {
        pinned_covers(&chain(mine), &pinned.encode().unwrap())
    }

    #[test]
    fn the_same_record_or_one_that_extends_it_covers_this_devices() {
        assert!(covers(&[1, 2, 3], &chain(&[1, 2, 3])));
        assert!(covers(&[1, 2], &chain(&[1, 2, 3, 4])));
        assert!(covers(&[1], &chain(&[1, 2])));
    }

    /// A pin that stops short of this device's chain is behind: replaced.
    #[test]
    fn a_shorter_record_does_not_cover() {
        assert!(!covers(&[1, 2, 3], &chain(&[1, 2])));
        assert!(!covers(&[1, 2], &chain(&[1])));
    }

    /// Two chains from one genesis that rotated to different successors: the
    /// pin is not this device's history, however long it is.
    #[test]
    fn a_divergent_record_does_not_cover_even_when_longer() {
        assert!(!covers(&[1, 2, 3], &chain(&[1, 2, 7])));
        assert!(!covers(&[1, 2, 3], &chain(&[1, 5, 6, 7])));
    }

    #[test]
    fn another_accounts_record_does_not_cover() {
        assert!(!covers(&[1, 2], &chain(&[8, 2, 3])));
    }

    /// A pin whose link does not verify is not kept, even where every link
    /// this device holds matches it.
    #[test]
    fn an_unverifiable_record_does_not_cover() {
        let mut forged = chain(&[1, 2, 3]);
        forged.links[1].body.roster_digest = [7; 32];
        assert!(forged.verify(&chain(&[1]).genesis_identity_id()).is_err());
        assert!(!covers(&[1, 2], &forged));
    }

    #[test]
    fn bytes_that_are_not_a_record_do_not_cover() {
        let mine = chain(&[1, 2]);
        assert!(!pinned_covers(&mine, &[]));
        assert!(!pinned_covers(&mine, b"not a chain record"));
        let mut cut = chain(&[1, 2, 3]).encode().unwrap();
        cut.truncate(cut.len() - 1);
        assert!(!pinned_covers(&mine, &cut));
    }
}
