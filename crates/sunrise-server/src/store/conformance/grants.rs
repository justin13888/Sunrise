//! The relay viewer grants every [`MetadataStore`] keeps (ADR-0074): one
//! claim binds one other account, either party withdraws, the cap counts only
//! live grants, and erasure takes the grant with either account.

use super::{subject, NOW};
use crate::store::{GrantCreated, MetadataStore, ViewerGrant, MAX_GRANTS_PER_OWNER};

fn grant(owner: &str, id: u8, stream: u8, expires_at_ms: Option<u64>) -> ViewerGrant {
    ViewerGrant {
        grant_id: [id; 16],
        owner_id: owner.to_owned(),
        stream_id: [stream; 16],
        recipient_id: None,
        expires_at_ms,
        created_ms: NOW,
        claimed_ms: None,
    }
}

async fn account(store: &dyn MetadataStore, name: &str) -> String {
    store
        .resolve_account(&subject(name), true, NOW)
        .await
        .unwrap()
        .account_id
}

/// The first other account to present the token is bound, and is answered
/// again; the owner, a third account, an unknown token and an expired grant
/// all get the same `None`. A retried registration is answered with the grant
/// it made, and a token another grant holds is refused.
pub(super) async fn a_grant_is_claimed_once_by_another_account(store: &dyn MetadataStore) {
    let owner = account(store, "grant-owner").await;
    let viewer = account(store, "grant-viewer").await;
    let third = account(store, "grant-third").await;
    let g = grant(&owner, 0x61, 0x71, Some(NOW + 1_000));
    let claim = [0x81; 32];

    assert_eq!(
        store.create_viewer_grant(&g, &claim, NOW).await.unwrap(),
        GrantCreated::Created(g.clone())
    );
    assert_eq!(
        store.create_viewer_grant(&g, &claim, NOW).await.unwrap(),
        GrantCreated::Exists(g.clone()),
        "a retried registration answers the grant it made"
    );
    let other_stream = grant(&owner, 0x62, 0x72, Some(NOW + 1_000));
    assert_eq!(
        store
            .create_viewer_grant(&other_stream, &claim, NOW)
            .await
            .unwrap(),
        GrantCreated::TokenTaken
    );
    assert_eq!(store.viewer_grant(&[0x62; 16]).await.unwrap(), None);

    assert_eq!(
        store.claim_viewer_grant(&claim, &owner, NOW).await.unwrap(),
        None,
        "the owner cannot claim its own grant"
    );
    assert_eq!(
        store
            .claim_viewer_grant(&[0x99; 32], &viewer, NOW)
            .await
            .unwrap(),
        None,
        "no grant under that token"
    );
    let claimed = store
        .claim_viewer_grant(&claim, &viewer, NOW + 1)
        .await
        .unwrap()
        .expect("the first other account binds");
    assert_eq!(claimed.recipient_id.as_deref(), Some(viewer.as_str()));
    assert_eq!(claimed.claimed_ms, Some(NOW + 1));
    assert!(claimed.serves(&viewer, NOW + 1));
    assert!(!claimed.serves(&third, NOW + 1));
    assert!(
        !claimed.serves(&viewer, NOW + 1_000),
        "expired at expires_at_ms"
    );
    assert_eq!(
        store.viewer_grant(&g.grant_id).await.unwrap(),
        Some(claimed.clone())
    );
    assert_eq!(
        store
            .claim_viewer_grant(&claim, &viewer, NOW + 2)
            .await
            .unwrap(),
        Some(claimed.clone()),
        "the holder's later claim is answered again, unchanged"
    );
    assert_eq!(
        store
            .claim_viewer_grant(&claim, &third, NOW + 2)
            .await
            .unwrap(),
        None,
        "a claimed grant is bound to one account"
    );

    let late = grant(&owner, 0x63, 0x73, Some(NOW + 10));
    store
        .create_viewer_grant(&late, &[0x83; 32], NOW)
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_viewer_grant(&[0x83; 32], &viewer, NOW + 10)
            .await
            .unwrap(),
        None,
        "an expired grant cannot be claimed"
    );
}

/// Each party sees the grant in its list; a third account sees nothing, and
/// cannot withdraw it; the recipient's withdrawal removes it for both.
pub(super) async fn a_grant_is_listed_and_withdrawn_by_either_party(store: &dyn MetadataStore) {
    let owner = account(store, "list-owner").await;
    let viewer = account(store, "list-viewer").await;
    let third = account(store, "list-third").await;
    let first = grant(&owner, 0x41, 0x51, None);
    let second = grant(&owner, 0x42, 0x52, None);
    for (g, claim) in [(&first, [0x91; 32]), (&second, [0x92; 32])] {
        store.create_viewer_grant(g, &claim, NOW).await.unwrap();
    }
    store
        .claim_viewer_grant(&[0x91; 32], &viewer, NOW)
        .await
        .unwrap()
        .unwrap();

    let owned = store.viewer_grants(&owner).await.unwrap();
    assert_eq!(
        owned.iter().map(|g| g.grant_id).collect::<Vec<_>>(),
        vec![[0x41; 16], [0x42; 16]]
    );
    let held = store.viewer_grants(&viewer).await.unwrap();
    assert_eq!(
        held.iter().map(|g| g.grant_id).collect::<Vec<_>>(),
        vec![[0x41; 16]]
    );
    assert!(store.viewer_grants(&third).await.unwrap().is_empty());

    assert!(!store
        .withdraw_viewer_grant(&[0x41; 16], &third)
        .await
        .unwrap());
    assert!(!store
        .withdraw_viewer_grant(&[0x42; 16], &viewer)
        .await
        .unwrap());
    assert!(store
        .withdraw_viewer_grant(&[0x41; 16], &viewer)
        .await
        .unwrap());
    assert_eq!(store.viewer_grant(&[0x41; 16]).await.unwrap(), None);
    assert!(store
        .withdraw_viewer_grant(&[0x42; 16], &owner)
        .await
        .unwrap());
    assert!(store.viewer_grants(&owner).await.unwrap().is_empty());
}

/// The cap counts live grants: at the cap a registration is refused, and once
/// the owner's grants have expired the next registration deletes them and
/// succeeds.
pub(super) async fn expired_grants_do_not_count_toward_the_cap(store: &dyn MetadataStore) {
    let owner = account(store, "cap-owner").await;
    for i in 0..MAX_GRANTS_PER_OWNER {
        let mut g = grant(&owner, 0, 0x21, Some(NOW + 100));
        g.grant_id[..8].copy_from_slice(&i.to_be_bytes());
        g.grant_id[8] = 0xca;
        let mut claim = [0xca; 32];
        claim[..8].copy_from_slice(&i.to_be_bytes());
        assert!(matches!(
            store.create_viewer_grant(&g, &claim, NOW).await.unwrap(),
            GrantCreated::Created(_)
        ));
    }
    let extra = grant(&owner, 0xcb, 0x21, None);
    assert_eq!(
        store
            .create_viewer_grant(&extra, &[0xcb; 32], NOW)
            .await
            .unwrap(),
        GrantCreated::TooMany
    );
    assert!(matches!(
        store
            .create_viewer_grant(&extra, &[0xcb; 32], NOW + 100)
            .await
            .unwrap(),
        GrantCreated::Created(_)
    ));
    assert_eq!(
        store.viewer_grants(&owner).await.unwrap(),
        vec![extra],
        "the expired grants went with the registration that needed room"
    );
}

/// Erasing the owner erases its grants; erasing the recipient erases the
/// grants it holds; neither touches a grant between two other accounts.
pub(super) async fn a_grant_goes_with_either_account(store: &dyn MetadataStore) {
    let owner = account(store, "erase-owner").await;
    let viewer = account(store, "erase-viewer").await;
    let kept_owner = account(store, "erase-kept-owner").await;
    let kept_viewer = account(store, "erase-kept-viewer").await;
    let by_owner = grant(&owner, 0x31, 0x01, None);
    let by_viewer = grant(&kept_owner, 0x32, 0x02, None);
    let kept = grant(&kept_owner, 0x33, 0x03, None);
    for (g, claim, holder) in [
        (&by_owner, [0xa1; 32], &kept_viewer),
        (&by_viewer, [0xa2; 32], &viewer),
        (&kept, [0xa3; 32], &kept_viewer),
    ] {
        store.create_viewer_grant(g, &claim, NOW).await.unwrap();
        store
            .claim_viewer_grant(&claim, holder, NOW)
            .await
            .unwrap()
            .unwrap();
    }

    assert!(store.erase_account(&owner).await.unwrap());
    assert_eq!(store.viewer_grant(&[0x31; 16]).await.unwrap(), None);
    assert!(store.erase_account(&viewer).await.unwrap());
    assert_eq!(store.viewer_grant(&[0x32; 16]).await.unwrap(), None);
    assert!(store.viewer_grant(&[0x33; 16]).await.unwrap().is_some());
}
