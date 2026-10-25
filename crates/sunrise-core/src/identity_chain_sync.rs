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
