//! Reinstatement on the word of the recorded revoker (ADR-0056 §5): the
//! signing key a revoke-by-vault-id records, and the only key whose
//! reinstatement the rows then accept. Split out of the suite because it is
//! one rule over the device rows, and the suite's own revocation check names
//! the relay id route.

use super::{subject, NOW};
use crate::store::{MetadataError, MetadataStore, NewDevice, Reinstatement, SignedBy};

const PHONE: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
const TABLET: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W82";
const LAPTOP: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";

/// The `Date` a revoke is signed under, and one a second later, both on the
/// signing device's clock and whole seconds as the header carries them.
const SIGNED: u64 = 1_800_000_000_000;
const LATER: u64 = SIGNED + 1_000;

fn by(key: &str, at_ms: u64) -> SignedBy<'_> {
    SignedBy { key, at_ms }
}

fn device(nickname: &str, vault_device_id: &str) -> NewDevice {
    NewDevice {
        device_pub_s: format!("key-{nickname}"),
        device_pub_d: None,
        device_cert: None,
        vault_device_id: Some(vault_device_id.to_owned()),
        nickname: nickname.to_owned(),
        platform: "linux".to_owned(),
        app_version: None,
    }
}

/// The recorded revoker reinstates, every row it revoked, idempotently; no
/// other device does, and another account reaches nothing.
pub(super) async fn only_the_recorded_revoker_reinstates(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("reinstate-alice"), true, NOW)
        .await
        .unwrap();
    let bob = store
        .resolve_account(&subject("reinstate-bob"), true, NOW)
        .await
        .unwrap();
    let account = alice.account_id.as_str();
    // Two rows for one vault id: a device that re-registered.
    let first = store
        .register_device(account, &device("phone", PHONE), NOW)
        .await
        .unwrap();
    let second = store
        .register_device(account, &device("phone", PHONE), NOW)
        .await
        .unwrap();
    let laptop = "key-laptop";
    let desktop = "key-desktop";

    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(account, PHONE, by(laptop, LATER))
                .await,
            Ok(Reinstatement::AlreadyActive)
        ),
        "a device nobody revoked is already accepted"
    );

    assert_eq!(
        store
            .revoke_devices_by_vault_id(account, PHONE, Some(by(laptop, SIGNED)), NOW + 1)
            .await
            .unwrap(),
        2
    );
    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(account, PHONE, by(desktop, LATER))
                .await,
            Ok(Reinstatement::NotRevoker)
        ),
        "a device that did not revoke it cannot reinstate it"
    );
    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(&bob.account_id, PHONE, by(laptop, LATER))
                .await,
            Err(MetadataError::NotFound)
        ),
        "another account's rows are not there to reinstate"
    );
    assert_eq!(
        store.active_device_count(account).await.unwrap(),
        0,
        "a refused reinstatement changed nothing"
    );

    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, PHONE, by(laptop, LATER))
            .await,
        Ok(Reinstatement::Reinstated(2))
    ));
    for id in [&first.device_id, &second.device_id] {
        assert!(
            store.active_device(account, id).await.unwrap().is_some(),
            "every row the revoker revoked is active again"
        );
    }
    let listed = store.list_devices(account).await.unwrap();
    assert!(
        listed
            .iter()
            .all(|d| !d.revoked && d.revoked_at_ms.is_none()),
        "a reinstated row reads as never revoked: {listed:?}"
    );
    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(account, PHONE, by(laptop, LATER))
                .await,
            Ok(Reinstatement::AlreadyActive)
        ),
        "a retry after it landed is not a refusal"
    );

    // The revoker is cleared on reinstatement, so a later revoke records its
    // own author and the earlier one no longer speaks for the rows.
    store
        .revoke_devices_by_vault_id(account, PHONE, Some(by(desktop, SIGNED)), NOW + 2)
        .await
        .unwrap();
    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, PHONE, by(laptop, LATER))
            .await,
        Ok(Reinstatement::NotRevoker)
    ));
}

/// A revoke that recorded no device leaves nobody to reinstate it, and a
/// vault id no row carries is not found.
pub(super) async fn an_unrecorded_revoke_cannot_be_reinstated(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("reinstate-unrecorded"), true, NOW)
        .await
        .unwrap();
    let account = alice.account_id.as_str();
    let laptop = "key-laptop";
    store
        .register_device(account, &device("tablet", TABLET), NOW)
        .await
        .unwrap();
    store
        .revoke_devices_by_vault_id(account, TABLET, None, NOW + 3)
        .await
        .unwrap();
    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, TABLET, by(laptop, LATER))
            .await,
        Ok(Reinstatement::NotRevoker)
    ));

    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, "01J8ZQ7X9K3M5N7P9R1T3V5W84", by(laptop, LATER))
            .await,
        Err(MetadataError::NotFound)
    ));
}

/// A reinstatement must be signed after the revocation it undoes. One the
/// revoker signed before revoking the device again, replayed inside the
/// signature's window, changes nothing, and nor does one signed in the same
/// second as the revoke; the same revoker signing later still succeeds.
pub(super) async fn a_reinstatement_signed_before_the_revoke_changes_nothing(
    store: &dyn MetadataStore,
) {
    let alice = store
        .resolve_account(&subject("reinstate-replay"), true, NOW)
        .await
        .unwrap();
    let account = alice.account_id.as_str();
    let laptop = "key-laptop";
    let first = store
        .register_device(account, &device("laptop", LAPTOP), NOW)
        .await
        .unwrap();

    // Revoked at SIGNED, reinstated at LATER: the request an eavesdropper
    // captured.
    store
        .revoke_devices_by_vault_id(account, LAPTOP, Some(by(laptop, SIGNED)), NOW + 1)
        .await
        .unwrap();
    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, LAPTOP, by(laptop, LATER))
            .await,
        Ok(Reinstatement::Reinstated(1))
    ));

    // The revoker revokes it again a second later, and a second row appears
    // for the device before that, so a replay that reinstated only the older
    // rows would still let the device back in.
    let second = store
        .register_device(account, &device("laptop", LAPTOP), NOW)
        .await
        .unwrap();
    let again = LATER + 1_000;
    store
        .revoke_devices_by_vault_id(account, LAPTOP, Some(by(laptop, again)), NOW + 2)
        .await
        .unwrap();

    for replayed_at in [LATER, again] {
        assert!(
            matches!(
                store
                    .reinstate_devices_by_vault_id(account, LAPTOP, by(laptop, replayed_at))
                    .await,
                Ok(Reinstatement::PredatesRevocation)
            ),
            "a reinstatement signed at {replayed_at} does not undo a revoke signed at {again}"
        );
    }
    for id in [&first.device_id, &second.device_id] {
        assert!(
            store.active_device(account, id).await.unwrap().is_none(),
            "a refused replay left every row revoked"
        );
    }

    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, LAPTOP, by(laptop, again + 1_000))
            .await,
        Ok(Reinstatement::Reinstated(2))
    ));

    // Rows of one vault id revoked under different dates by one revoker are no
    // longer reachable through registration: a third row registered after the
    // first two were revoked is refused (#1300). The store still reinstates
    // all or nothing, for rows that predate that refusal.
}

/// A revoked device cannot register its way back (#1300): not with its key,
/// not with a fresh key under its vault id, and not with its key under a fresh
/// vault id. The refusal writes nothing, reaches no other account, and ends
/// once the revoker reinstates the device.
pub(super) async fn a_revoked_device_cannot_register_again(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("reregister-alice"), true, NOW)
        .await
        .unwrap();
    let bob = store
        .resolve_account(&subject("reregister-bob"), true, NOW)
        .await
        .unwrap();
    let account = alice.account_id.as_str();
    let laptop = "key-laptop";
    let phone = device("phone", PHONE);
    let first = store.register_device(account, &phone, NOW).await.unwrap();
    store
        .register_device(account, &device("tablet", TABLET), NOW)
        .await
        .unwrap();
    store
        .revoke_devices_by_vault_id(account, PHONE, Some(by(laptop, SIGNED)), NOW + 1)
        .await
        .unwrap();
    let before = store.list_devices(account).await.unwrap();

    let fresh_key = NewDevice {
        device_pub_s: "key-phone-fresh".to_owned(),
        ..phone.clone()
    };
    let fresh_vault_id = NewDevice {
        vault_device_id: Some(LAPTOP.to_owned()),
        ..phone.clone()
    };
    let no_vault_id = NewDevice {
        vault_device_id: None,
        ..phone.clone()
    };
    for (what, attempt) in [
        ("its key and vault id", &phone),
        ("a fresh key under its vault id", &fresh_key),
        ("its key under a fresh vault id", &fresh_vault_id),
        ("its key and no vault id", &no_vault_id),
    ] {
        assert!(
            matches!(
                store.register_device(account, attempt, NOW + 2).await,
                Err(MetadataError::DeviceRevoked)
            ),
            "a revoked device registering {what} is refused"
        );
    }
    assert_eq!(
        store.list_devices(account).await.unwrap(),
        before,
        "a refused registration wrote nothing"
    );

    // An active device registering again is still a second row, and another
    // account holding the same key is not this account's revoked device.
    store
        .register_device(account, &device("tablet", TABLET), NOW + 2)
        .await
        .unwrap();
    store
        .register_device(&bob.account_id, &phone, NOW + 2)
        .await
        .unwrap();

    // The revoker's reinstatement leaves no revoked row behind, and the device
    // registers again.
    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, PHONE, by(laptop, LATER))
            .await,
        Ok(Reinstatement::Reinstated(1))
    ));
    let again = store
        .register_device(account, &phone, NOW + 3)
        .await
        .unwrap();
    assert_ne!(again.device_id, first.device_id);
    assert!(store
        .active_device(account, &again.device_id)
        .await
        .unwrap()
        .is_some());
}
