//! The pins every [`MetadataStore`] keeps outside op retention: one snapshot
//! record per stream (ADR-0068) and one key bundle per account (ADR-0069).
//! Split out of the suite because both are pointers into the blob backend and
//! share nothing with the rest of it.

use super::{subject, NOW};
use crate::store::{KeyBundlePinned, MetadataStore, PinnedKeyBundle, PinnedSnapshot};

/// A pin replaces its stream's last one and hands back the blob it released;
/// a blob another of the account's streams still names is not released, and
/// no other account sees the pin.
pub(super) async fn one_snapshot_is_pinned_per_stream(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("snap-alice"), true, NOW)
        .await
        .unwrap();
    let bob = store
        .resolve_account(&subject("snap-bob"), true, NOW)
        .await
        .unwrap();
    let pin = |stream: u8, blob: u8, at: u64| PinnedSnapshot {
        stream_id: [stream; 16],
        blob_key: [blob; 16],
        size_bytes: 100 + u64::from(blob),
        published_by: Some("dev".to_owned()),
        published_at_ms: at,
    };
    let a = &alice.account_id;
    assert_eq!(store.pinned_snapshot(a, &[1; 16]).await.unwrap(), None);
    assert_eq!(
        store.pin_snapshot(a, &pin(1, 0xa0, NOW)).await.unwrap(),
        None
    );
    assert_eq!(
        store.pinned_snapshot(a, &[1; 16]).await.unwrap(),
        Some(pin(1, 0xa0, NOW))
    );
    assert_eq!(
        store.pin_snapshot(a, &pin(1, 0xa0, NOW + 1)).await.unwrap(),
        None,
        "pinning the same blob again releases nothing"
    );
    assert_eq!(
        store.pin_snapshot(a, &pin(1, 0xa1, NOW + 2)).await.unwrap(),
        Some([0xa0; 16]),
        "a newer pin releases the blob the older one named"
    );
    assert_eq!(
        store.pinned_snapshot(a, &[1; 16]).await.unwrap(),
        Some(pin(1, 0xa1, NOW + 2))
    );
    assert_eq!(
        store.pin_snapshot(a, &pin(2, 0xa1, NOW)).await.unwrap(),
        None
    );
    assert_eq!(
        store.pin_snapshot(a, &pin(1, 0xa2, NOW + 3)).await.unwrap(),
        None,
        "a blob stream 2 still names is not released"
    );
    assert_eq!(
        store
            .pinned_snapshot(&bob.account_id, &[1; 16])
            .await
            .unwrap(),
        None,
        "a pin is the account's own"
    );
}

/// A key bundle pin replaces only the pin its publisher read: a publisher that
/// read a stale one is told so and writes nothing, and no other account sees
/// the pin.
pub(super) async fn one_key_bundle_is_pinned_per_account(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("bundle-alice"), true, NOW)
        .await
        .unwrap();
    let bob = store
        .resolve_account(&subject("bundle-bob"), true, NOW)
        .await
        .unwrap();
    let pin = |blob: u8, at: u64| PinnedKeyBundle {
        blob_key: [blob; 16],
        size_bytes: 100 + u64::from(blob),
        manifest_digest: [blob; 32],
        published_by: Some("dev".to_owned()),
        published_at_ms: at,
    };
    let a = &alice.account_id;
    assert_eq!(store.pinned_key_bundle(a).await.unwrap(), None);
    assert_eq!(
        store
            .pin_key_bundle(a, &pin(0xb0, NOW), Some([0xff; 16]))
            .await
            .unwrap(),
        KeyBundlePinned::Superseded,
        "nothing is pinned, so a publisher naming a pin read a stale one"
    );
    assert_eq!(
        store
            .pin_key_bundle(a, &pin(0xb0, NOW), None)
            .await
            .unwrap(),
        KeyBundlePinned::Pinned { released: None }
    );
    assert_eq!(
        store.pinned_key_bundle(a).await.unwrap(),
        Some(pin(0xb0, NOW))
    );
    assert_eq!(
        store
            .pin_key_bundle(a, &pin(0xb1, NOW + 1), None)
            .await
            .unwrap(),
        KeyBundlePinned::Superseded,
        "a publisher that read no pin does not overwrite one"
    );
    assert_eq!(
        store
            .pin_key_bundle(a, &pin(0xb1, NOW + 2), Some([0xb0; 16]))
            .await
            .unwrap(),
        KeyBundlePinned::Pinned {
            released: Some([0xb0; 16])
        }
    );
    assert_eq!(
        store
            .pin_key_bundle(a, &pin(0xb2, NOW + 3), Some([0xb0; 16]))
            .await
            .unwrap(),
        KeyBundlePinned::Superseded,
        "the pin a racing publisher read has been replaced"
    );
    assert_eq!(
        store.pinned_key_bundle(a).await.unwrap(),
        Some(pin(0xb1, NOW + 2))
    );
    assert_eq!(
        store.pinned_key_bundle(&bob.account_id).await.unwrap(),
        None,
        "a pin is the account's own"
    );
}

/// An account's key bundle pin goes with the account, and another account's
/// stays.
pub(super) async fn a_key_bundle_goes_with_its_account(store: &dyn MetadataStore) {
    let pin = PinnedKeyBundle {
        blob_key: [0x45; 16],
        size_bytes: 1,
        manifest_digest: [0; 32],
        published_by: None,
        published_at_ms: NOW,
    };
    let gone = store
        .resolve_account(&subject("bundle-gone"), true, NOW)
        .await
        .unwrap();
    let kept = store
        .resolve_account(&subject("bundle-kept"), true, NOW)
        .await
        .unwrap();
    for account in [&gone, &kept] {
        store
            .pin_key_bundle(&account.account_id, &pin, None)
            .await
            .unwrap();
    }
    assert!(store.erase_account(&gone.account_id).await.unwrap());
    assert_eq!(
        store.pinned_key_bundle(&gone.account_id).await.unwrap(),
        None
    );
    assert_eq!(
        store.pinned_key_bundle(&kept.account_id).await.unwrap(),
        Some(pin)
    );
}
