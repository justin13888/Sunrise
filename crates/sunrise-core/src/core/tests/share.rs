//! A Stream shared from one account and opened by another, through `Core`.

use super::*;
use sunrise_crypto::ShareGrantError;
use sunrise_domain::StreamDraft;

async fn account(dir: &std::path::Path) -> Core {
    Core::open(cfg(dir), unlock()).await.unwrap()
}

async fn stream(core: &Core, name: &str) -> sunrise_id::EntityRef {
    core.submit(Command::CreateStream(StreamDraft {
        name: name.into(),
        ..Default::default()
    }))
    .await
    .unwrap()
    .entity
}

fn refused_as(e: &CoreError, expected: &ShareGrantError) -> bool {
    matches!(e, CoreError::Keychain(KeychainError::ShareGrant(got)) if got == expected)
}

#[tokio::test]
async fn a_share_opens_for_its_recipient_with_the_streams_name() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    assert_ne!(owner.identity_id(), recipient.identity_id());
    let s = stream(&owner, "Garden").await;

    let expires = owner.now_ms() + 86_400_000;
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            Some(expires),
        )
        .unwrap();
    let opened = recipient
        .open_share_grant(&owner.identity_signing_pub(), &grant)
        .unwrap();

    assert_eq!(opened.stream_id, *s.bytes());
    assert_eq!(opened.name, "Garden");
    assert_eq!(opened.granter_identity_id, owner.identity_id());
    assert_eq!(opened.expires_at_ms, Some(expires));
}

/// The op names the recipient identity's Person, so the owner's share fold
/// lists the share under the identity the grant was sealed to.
#[tokio::test]
async fn the_share_is_in_force_on_the_owners_replica() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner, "Garden").await;
    owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            None,
        )
        .unwrap();

    let shares = {
        let db = owner.db.lock();
        owner.engine.stream_shares(&db, s).unwrap()
    };
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].recipient_identity_id, recipient.identity_id());
    assert_eq!(*shares[0].person.bytes(), recipient.identity_id());
    assert_eq!(shares[0].person.kind(), sunrise_id::EntityKind::Person);
}

/// A third account cannot open a grant sealed to someone else, and a grant
/// checked against any key but the granter's is a forgery.
#[tokio::test]
async fn a_grant_opens_for_its_recipient_under_its_granter_only() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let owner = account(dirs[0].path()).await;
    let recipient = account(dirs[1].path()).await;
    let other = account(dirs[2].path()).await;
    let s = stream(&owner, "Garden").await;
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            None,
        )
        .unwrap();

    let e = other
        .open_share_grant(&owner.identity_signing_pub(), &grant)
        .unwrap_err();
    assert!(refused_as(&e, &ShareGrantError::OtherRecipient));

    let e = recipient
        .open_share_grant(&other.identity_signing_pub(), &grant)
        .unwrap_err();
    assert!(refused_as(&e, &ShareGrantError::BadSignature));

    assert!(matches!(
        recipient.open_share_grant(&owner.identity_signing_pub(), b"not a grant"),
        Err(CoreError::Keychain(KeychainError::ShareGrant(_)))
    ));
}

/// A Stream is not shared with the account's own identity, and nothing is
/// written when the engine refuses.
#[tokio::test]
async fn sharing_with_ones_own_identity_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let owner = account(dir.path()).await;
    let s = stream(&owner, "Garden").await;
    let before = vault_footprint(&owner);
    let refused = owner.share_stream(
        s,
        &owner.identity_signing_pub(),
        owner.identity_dh_pub(),
        None,
    );
    assert!(
        matches!(refused, Err(CoreError::Engine(EngineError::Invalid(_)))),
        "{refused:?}"
    );
    assert_eq!(vault_footprint(&owner), before);
}

/// Shares `owner`'s Stream `s` with `recipient`, for good.
fn share_with(owner: &Core, s: sunrise_id::EntityRef, recipient: &Core) {
    owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            None,
        )
        .unwrap();
}

/// A revocation through `Core` ends the share, lists it as revoked, and
/// rotates the Stream past the epoch the recipient was sealed.
#[tokio::test]
async fn revoking_a_share_ends_it_and_rotates_the_stream() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner, "Garden").await;
    share_with(&owner, s, &recipient);
    let sealed = owner.stream_shares(s).unwrap()[0].epoch;

    let minted = owner.revoke_share(s, recipient.identity_id()).unwrap();

    assert!(minted > sealed, "{minted} after {sealed}");
    assert!(owner.stream_shares(s).unwrap().is_empty());
    let ended = owner.ended_stream_shares(s).unwrap();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].recipient_identity_id, recipient.identity_id());
    assert_eq!(ended[0].end, crate::ShareEnd::Revoked);
}

/// A decline through `Core` ends the share as declined, and rotates nothing.
#[tokio::test]
async fn recording_a_decline_ends_the_share_without_a_rotation() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner, "Garden").await;
    share_with(&owner, s, &recipient);
    let keys_before = vault_footprint(&owner).2;

    owner
        .record_share_decline(s, recipient.identity_id())
        .unwrap();

    assert!(owner.stream_shares(s).unwrap().is_empty());
    let ended = owner.ended_stream_shares(s).unwrap();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].recipient_identity_id, recipient.identity_id());
    assert_eq!(ended[0].end, crate::ShareEnd::Declined);
    assert_eq!(vault_footprint(&owner).2, keys_before, "no epoch minted");
}

/// Ending a share nobody holds is refused, and nothing is written.
#[tokio::test]
async fn ending_a_share_not_in_force_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let owner = account(dir.path()).await;
    let s = stream(&owner, "Garden").await;
    let before = vault_footprint(&owner);

    let revoked = owner.revoke_share(s, [9; 16]);
    assert!(
        matches!(revoked, Err(CoreError::Engine(EngineError::Invalid(_)))),
        "{revoked:?}"
    );
    let declined = owner.record_share_decline(s, [9; 16]);
    assert!(
        matches!(declined, Err(CoreError::Engine(EngineError::Invalid(_)))),
        "{declined:?}"
    );
    assert_eq!(vault_footprint(&owner), before);
    assert!(owner.ended_stream_shares(s).unwrap().is_empty());
}

/// Accepting a share holds the Stream key it sealed, under the granter, and
/// nowhere this account's own keys are: `stream_keys` and the key cache are
/// untouched, and no op is written.
#[tokio::test]
async fn an_accepted_share_holds_the_owners_key_apart_from_the_accounts_own() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner, "Garden").await;
    let expires = owner.now_ms() + 86_400_000;
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            Some(expires),
        )
        .unwrap();
    let before = vault_footprint(&recipient);

    let accepted = recipient
        .accept_share(&owner.identity_signing_pub(), &grant, [0x5a; 16])
        .unwrap();

    assert_eq!(accepted.stream_id, *s.bytes());
    assert_eq!(vault_footprint(&recipient), before, "no op, no own key");
    let keychain = recipient.engine.keychain();
    assert!(keychain
        .stream_keys_at(s.bytes(), accepted.epoch)
        .is_empty());
    let held = {
        let db = recipient.db.lock();
        keychain
            .held_share_key(&db, &owner.identity_id(), s.bytes(), accepted.epoch)
            .unwrap()
            .expect("the key is held")
    };
    let owners = owner
        .engine
        .keychain()
        .stream_keys_at(s.bytes(), accepted.epoch);
    assert!(
        owners.iter().any(|k| k.as_bytes() == held.as_bytes()),
        "the held key is the owner's key at the sealed epoch"
    );

    let listed = recipient.held_shares().unwrap();
    assert_eq!(listed.len(), 1);
    let share = &listed[0];
    assert_eq!(share.granter_identity_id, owner.identity_id());
    assert_eq!(share.stream_id, *s.bytes());
    assert_eq!(share.epoch, accepted.epoch);
    assert_eq!(share.name, "Garden");
    assert_eq!(share.expires_at_ms, Some(expires));
    assert_eq!(share.relay_grant_ids, vec![[0x5a; 16]]);
}

/// A grant that does not open for this account holds nothing.
#[tokio::test]
async fn a_refused_grant_is_not_held() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let owner = account(dirs[0].path()).await;
    let recipient = account(dirs[1].path()).await;
    let other = account(dirs[2].path()).await;
    let s = stream(&owner, "Garden").await;
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            None,
        )
        .unwrap();

    let e = other
        .accept_share(&owner.identity_signing_pub(), &grant, [1; 16])
        .unwrap_err();
    assert!(refused_as(&e, &ShareGrantError::OtherRecipient));
    let e = recipient
        .accept_share(&other.identity_signing_pub(), &grant, [1; 16])
        .unwrap_err();
    assert!(refused_as(&e, &ShareGrantError::BadSignature));
    assert!(other.held_shares().unwrap().is_empty());
    assert!(recipient.held_shares().unwrap().is_empty());
}

/// Accepting the same epoch again records the relay grant claimed last, and
/// a decline's drop removes the share once.
#[tokio::test]
async fn a_held_share_is_replaced_by_a_second_accept_and_dropped_once() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (owner, recipient) = (account(da.path()).await, account(db.path()).await);
    let s = stream(&owner, "Garden").await;
    let grant = owner
        .share_stream(
            s,
            &recipient.identity_signing_pub(),
            recipient.identity_dh_pub(),
            None,
        )
        .unwrap();
    let granter = owner.identity_signing_pub();
    recipient.accept_share(&granter, &grant, [1; 16]).unwrap();
    recipient.accept_share(&granter, &grant, [2; 16]).unwrap();

    let listed = recipient.held_shares().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].relay_grant_ids, vec![[2; 16]]);

    assert!(recipient
        .drop_held_share(owner.identity_id(), *s.bytes())
        .unwrap());
    assert!(recipient.held_shares().unwrap().is_empty());
    assert!(!recipient
        .drop_held_share(owner.identity_id(), *s.bytes())
        .unwrap());
}

/// Epochs held for one (granter, stream) fold into one entry at the newest
/// epoch with the union of their relay grant ids, which `share decline`
/// relies on to withdraw every grant.
#[tokio::test]
async fn several_held_epochs_fold_into_one_share_with_every_relay_grant() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let owner = account(dirs[0].path()).await;
    let recipient = account(dirs[1].path()).await;
    let bystander = account(dirs[2].path()).await;
    let s = stream(&owner, "Garden").await;
    let granter = owner.identity_signing_pub();
    let seal = |to: &Core| {
        owner
            .share_stream(s, &to.identity_signing_pub(), to.identity_dh_pub(), None)
            .unwrap()
    };

    let first = seal(&recipient);
    let one = recipient
        .accept_share(&granter, &first, [0x22; 16])
        .unwrap();
    // Revoking another recipient rotates the Stream key to a newer epoch.
    seal(&bystander);
    let newer = owner.revoke_share(s, bystander.identity_id()).unwrap();
    let second = seal(&recipient);
    let two = recipient
        .accept_share(&granter, &second, [0x11; 16])
        .unwrap();
    assert!(two.epoch > one.epoch, "a rotation raised the epoch");
    assert_eq!(two.epoch, newer);

    let listed = recipient.held_shares().unwrap();
    assert_eq!(listed.len(), 1, "one entry per (granter, stream)");
    assert_eq!(listed[0].epoch, two.epoch, "the newest epoch");
    assert_eq!(
        listed[0].relay_grant_ids,
        vec![[0x11; 16], [0x22; 16]],
        "the union of both epochs' relay grants, in byte order"
    );
}

/// The owner's relay grant ids are recorded per share, listed in byte
/// order, kept once however often recorded, and forgotten one at a time.
#[tokio::test]
async fn an_owner_records_and_forgets_its_shares_relay_grants() {
    let dir = tempfile::tempdir().unwrap();
    let owner = account(dir.path()).await;
    let s = stream(&owner, "Garden").await;
    let other = stream(&owner, "Kitchen").await;
    let before = vault_footprint(&owner);

    owner.record_share_relay_grant(s, [9; 16], [2; 16]).unwrap();
    owner.record_share_relay_grant(s, [9; 16], [1; 16]).unwrap();
    owner.record_share_relay_grant(s, [9; 16], [1; 16]).unwrap();
    owner.record_share_relay_grant(s, [8; 16], [3; 16]).unwrap();

    assert_eq!(
        owner.share_relay_grants(s, [9; 16]).unwrap(),
        vec![[1; 16], [2; 16]]
    );
    assert_eq!(owner.share_relay_grants(s, [8; 16]).unwrap(), vec![[3; 16]]);
    assert!(owner.share_relay_grants(other, [9; 16]).unwrap().is_empty());

    owner.forget_share_relay_grant(s, [9; 16], [1; 16]).unwrap();
    assert_eq!(owner.share_relay_grants(s, [9; 16]).unwrap(), vec![[2; 16]]);
    assert_eq!(vault_footprint(&owner), before, "no op is written");
}
