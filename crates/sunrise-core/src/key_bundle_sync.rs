//! The account's key bundle, carried between a device and the relay
//! ([ADR-0069](../../../docs/11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)).
//!
//! Two legs, both driven by the sync driver over the one transport it holds,
//! as a snapshot record's are (`snapshot_sync`):
//!
//! - **Publish.** Before a session reports `Live`, and on the resync tick, a
//!   device compares the keys it may write under with the bundle the relay
//!   pins. It publishes only when it holds every key the pinned bundle
//!   carries and at least one more, or when the pinned bundle is not one to
//!   keep. The pin names the bundle it replaces, so of two devices racing the
//!   one that read a stale pin is refused and reads again. A rotation, a new
//!   stream and a revocation all add keys, so each is published by whichever
//!   device first holds the new set, normally the one that performed it.
//! - **Recover.** A device that holds `ID_D_priv` (one restored from the
//!   recovery code) and is told a stream's snapshot record needs a key it does
//!   not hold fetches the pinned bundle and absorbs it. That is what stands in
//!   for the identity-addressed envelopes the relay has trimmed.
//!
//! The relay can serve a stale bundle. Nothing here can tell, and ADR-0069
//! §Rollback says what that costs.

use sunrise_crypto::manifest_covers;
use sunrise_sync::{KeyBundlePinned, Transport, TransportError};

use crate::core::{Core, CoreError};
use crate::engine::{hex_short, EngineError, KeyBundlePlan, PinnedKeyBundle, META_STREAM};
use crate::snapshot_sync::{blob_id_of, upload};

/// What comparing this device's keys with the pinned bundle decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The pinned bundle already carries every key this device would publish.
    Covered,
    /// The pinned bundle lacks keys this device holds, and holds none this
    /// device lacks: replace it.
    Replace,
    /// Each holds a key the other lacks (two devices rotated concurrently).
    /// Wait until this device has absorbed the other's keys; it then holds a
    /// superset and replaces the pin.
    Wait,
}

/// Whether to replace a pinned bundle whose manifest is `pinned` with one
/// whose manifest is `mine`. A pin no honest device would keep is replaced
/// whatever it carries.
pub(crate) fn decide(mine: &[sunrise_crypto::ManifestEntry], pinned: &PinnedKeyBundle) -> Decision {
    match pinned {
        PinnedKeyBundle::Refused(_) => Decision::Replace,
        PinnedKeyBundle::Valid(theirs) if manifest_covers(theirs, mine) => Decision::Covered,
        PinnedKeyBundle::Valid(theirs) if manifest_covers(mine, theirs) => Decision::Replace,
        PinnedKeyBundle::Valid(_) => Decision::Wait,
    }
}

/// Publish this device's key bundle if the relay's is behind it.
///
/// Failures are logged and left for the next tick, which asks again.
pub(crate) async fn publish<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(Some(plan)) = core.key_bundle_plan() else {
        return;
    };
    match publish_plan(core, transport, &plan).await {
        Ok(()) | Err(TransportError::Unsupported) => {}
        Err(e) => tracing::warn!(
            ev = "sync.key_bundle.publish_failed",
            err_code = "SYNC_NETWORK_UNAVAILABLE",
            err_kind = "transient",
            retryable = true,
            cause = %e,
            "the account's key bundle did not reach the relay; the next tick tries again"
        ),
    }
}

async fn publish_plan<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
    plan: &KeyBundlePlan,
) -> Result<(), TransportError> {
    let pinned = transport.key_bundle_latest().await?;
    let replaces = match pinned {
        None => None,
        Some(pin) if pin.manifest_digest == plan.digest => {
            core.note_key_bundle_current(plan.digest);
            return Ok(());
        }
        Some(pin) => {
            let judged = match transport.blob_fetch(&pin.blob_id).await? {
                Some(bytes) if blob_id_of(&bytes) == pin.blob_id => core
                    .judge_pinned_key_bundle(&bytes)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?,
                // Replaced between the two reads; the next tick reads again.
                None => return Ok(()),
                Some(_) => PinnedKeyBundle::Refused("not_the_pinned_blob"),
            };
            match decide(&plan.manifest, &judged) {
                Decision::Covered => {
                    core.note_key_bundle_current(plan.digest);
                    return Ok(());
                }
                Decision::Wait => return Ok(()),
                Decision::Replace => {
                    if let PinnedKeyBundle::Refused(reason) = judged {
                        tracing::warn!(
                            ev = "sync.key_bundle.replaced_refused",
                            blob_h = hex_short(&pin.blob_id),
                            reason,
                            "the relay pins a key bundle this device would not keep; replacing it"
                        );
                    }
                    Some(pin.blob_id)
                }
            }
        }
    };
    let bytes = core
        .seal_key_bundle(plan)
        .map_err(|e| TransportError::Protocol(e.to_string()))?;
    let blob_id = upload(transport, &META_STREAM, &bytes).await?;
    match transport
        .key_bundle_pin(&blob_id, &plan.digest, replaces.as_ref())
        .await?
    {
        KeyBundlePinned::Pinned => {
            core.note_key_bundle_current(plan.digest);
            tracing::info!(
                ev = "sync.key_bundle.published",
                blob_h = hex_short(&blob_id),
                n_bytes = bytes.len() as u64,
                n_total = plan.keys.len() as u64,
                "the account's key bundle is pinned on the relay for a device restored from the recovery code"
            );
        }
        // Another device pinned first; the next tick judges its bundle.
        KeyBundlePinned::Superseded => {}
    }
    Ok(())
}

/// What fetching and absorbing the pinned bundle came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// It was absorbed and brought this many keys this vault lacked.
    Absorbed(usize),
    /// Nothing absorbed, for the reason named.
    Nothing(&'static str),
}

/// [`recover`], saying nothing when the relay pins no bundle or has no route
/// for one: the account may simply never have published one.
pub(crate) async fn recover_if_pinned<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    if matches!(transport.key_bundle_latest().await, Ok(Some(_))) {
        recover(core, transport).await;
    }
}

/// Fetch the account's pinned key bundle and absorb it.
pub(crate) async fn recover<T: Transport + ?Sized>(core: &Core, transport: &mut T) -> Recovery {
    let pin = match transport.key_bundle_latest().await {
        Ok(Some(pin)) => pin,
        Ok(None) => return Recovery::Nothing("no_bundle"),
        Err(TransportError::Unsupported) => return Recovery::Nothing("unsupported"),
        Err(_) => return Recovery::Nothing("transport"),
    };
    let bytes = match transport.blob_fetch(&pin.blob_id).await {
        Ok(Some(bytes)) if blob_id_of(&bytes) == pin.blob_id => bytes,
        Ok(Some(_)) => return Recovery::Nothing("rejected"),
        Ok(None) => return Recovery::Nothing("not_on_relay"),
        Err(_) => return Recovery::Nothing("transport"),
    };
    let outcome = match core.apply_key_bundle(&bytes) {
        Ok(applied) => Recovery::Absorbed(applied.absorbed),
        Err(CoreError::Engine(EngineError::Invalid(_))) => Recovery::Nothing("rejected"),
        Err(_) => Recovery::Nothing("unopenable"),
    };
    match outcome {
        Recovery::Absorbed(n) => tracing::info!(
            ev = "sync.key_bundle.recovered",
            blob_h = hex_short(&pin.blob_id),
            n_imported = n as u64,
            "keys the relay no longer had as envelopes were taken from the account's key bundle"
        ),
        Recovery::Nothing(reason) => tracing::warn!(
            ev = "sync.key_bundle.unavailable",
            blob_h = hex_short(&pin.blob_id),
            reason,
            "the account's key bundle could not be used; streams that need its keys stay behind"
        ),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sunrise_crypto::ManifestEntry;

    fn m(entries: &[(u8, u32)]) -> Vec<ManifestEntry> {
        entries
            .iter()
            .map(|(s, e)| ManifestEntry {
                stream_id: [*s; 16],
                epoch: *e,
                keys: 1,
            })
            .collect()
    }

    /// A device never replaces a pinned bundle with one that holds less: the
    /// rollback an honest publisher could otherwise cause by being behind.
    #[test]
    fn a_publisher_never_rolls_the_pin_back() {
        let newer = m(&[(0, 1), (0, 2), (7, 1)]);
        let older = m(&[(0, 1), (7, 1)]);
        assert_eq!(
            decide(&older, &PinnedKeyBundle::Valid(newer.clone())),
            Decision::Covered
        );
        assert_eq!(
            decide(&newer, &PinnedKeyBundle::Valid(older)),
            Decision::Replace
        );
        assert_eq!(
            decide(&newer, &PinnedKeyBundle::Valid(newer.clone())),
            Decision::Covered
        );
        // Two concurrent rotations: neither replaces the other until one
        // holds both.
        let a = m(&[(0, 1), (0, 2)]);
        let b = m(&[(0, 1), (7, 2)]);
        assert_eq!(
            decide(&a, &PinnedKeyBundle::Valid(b.clone())),
            Decision::Wait
        );
        assert_eq!(decide(&b, &PinnedKeyBundle::Valid(a)), Decision::Wait);
    }

    /// A pin no honest device would keep — forged, of another account, or by
    /// a device this replica has bounded — is replaced whatever it carries.
    #[test]
    fn a_refused_pin_is_replaced_even_by_less() {
        assert_eq!(
            decide(
                &m(&[(0, 1)]),
                &PinnedKeyBundle::Refused("publisher_bounded")
            ),
            Decision::Replace
        );
    }

    /// A manifest over a few streams and epochs, so generated pairs overlap
    /// often enough to land in every arm of [`decide`].
    fn manifest() -> impl Strategy<Value = Vec<ManifestEntry>> {
        proptest::collection::btree_map((0u8..3, 0u32..4), 1u32..3, 0..6).prop_map(|entries| {
            entries
                .into_iter()
                .map(|((s, epoch), keys)| ManifestEntry {
                    stream_id: [s; 16],
                    epoch,
                    keys,
                })
                .collect()
        })
    }

    proptest::proptest! {
        /// Whatever the two manifests, a device whose keys the pin covers
        /// never replaces it: a strict subset and an equal set are `Covered`,
        /// so no honest publisher rolls the pin back.
        #[test]
        fn a_covered_manifest_never_replaces_the_pin(mine in manifest(), theirs in manifest()) {
            let decision = decide(&mine, &PinnedKeyBundle::Valid(theirs.clone()));
            if manifest_covers(&theirs, &mine) {
                proptest::prop_assert_eq!(decision, Decision::Covered);
            }
            if decision == Decision::Replace {
                proptest::prop_assert!(manifest_covers(&mine, &theirs));
                proptest::prop_assert!(!manifest_covers(&theirs, &mine));
            }
        }

        /// Drop one entry from a manifest and the two are a strict subset and
        /// its superset: the superset replaces the subset's pin, and the
        /// subset leaves the superset's alone.
        #[test]
        fn a_strict_subset_is_replaced_and_never_replaces(
            full in manifest().prop_filter("non-empty", |m| !m.is_empty()),
            drop in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let mut subset = full.clone();
            subset.remove(drop.index(full.len()));
            proptest::prop_assert_eq!(
                decide(&full, &PinnedKeyBundle::Valid(subset.clone())),
                Decision::Replace
            );
            proptest::prop_assert_eq!(
                decide(&subset, &PinnedKeyBundle::Valid(full)),
                Decision::Covered
            );
        }

        /// Two manifests that each hold an entry the other lacks wait, from
        /// either side.
        #[test]
        fn manifests_that_each_hold_a_key_the_other_lacks_wait(
            shared in manifest(),
            ours in 0u32..4,
            theirs in 0u32..4,
        ) {
            let only = |stream: u8, epoch: u32| ManifestEntry { stream_id: [stream; 16], epoch, keys: 1 };
            let mut mine = shared.clone();
            mine.push(only(10, ours));
            let mut pinned = shared;
            pinned.push(only(11, theirs));
            proptest::prop_assert_eq!(
                decide(&mine, &PinnedKeyBundle::Valid(pinned.clone())),
                Decision::Wait
            );
            proptest::prop_assert_eq!(decide(&pinned, &PinnedKeyBundle::Valid(mine)), Decision::Wait);
        }
    }
}
