//! Reinstatement on the word of the recorded revoker (ADR-0056 §5): the
//! signing key a revoke-by-vault-id records, and the only key whose
//! reinstatement the rows then accept. Split out of the suite because it is
//! one rule over the device rows, and the suite's own revocation check names
//! the relay id route.

use super::{subject, NOW};
use crate::store::{MetadataError, MetadataStore, NewDevice, Reinstatement};

const PHONE: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
const TABLET: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W82";

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
                .reinstate_devices_by_vault_id(account, PHONE, laptop)
                .await,
            Ok(Reinstatement::AlreadyActive)
        ),
        "a device nobody revoked is already accepted"
    );

    assert_eq!(
        store
            .revoke_devices_by_vault_id(account, PHONE, Some(laptop), NOW + 1)
            .await
            .unwrap(),
        2
    );
    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(account, PHONE, desktop)
                .await,
            Ok(Reinstatement::NotRevoker)
        ),
        "a device that did not revoke it cannot reinstate it"
    );
    assert!(
        matches!(
            store
                .reinstate_devices_by_vault_id(&bob.account_id, PHONE, laptop)
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
            .reinstate_devices_by_vault_id(account, PHONE, laptop)
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
                .reinstate_devices_by_vault_id(account, PHONE, laptop)
                .await,
            Ok(Reinstatement::AlreadyActive)
        ),
        "a retry after it landed is not a refusal"
    );

    // The revoker is cleared on reinstatement, so a later revoke records its
    // own author and the earlier one no longer speaks for the rows.
    store
        .revoke_devices_by_vault_id(account, PHONE, Some(desktop), NOW + 2)
        .await
        .unwrap();
    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, PHONE, laptop)
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
            .reinstate_devices_by_vault_id(account, TABLET, laptop)
            .await,
        Ok(Reinstatement::NotRevoker)
    ));

    assert!(matches!(
        store
            .reinstate_devices_by_vault_id(account, "01J8ZQ7X9K3M5N7P9R1T3V5W84", laptop)
            .await,
        Err(MetadataError::NotFound)
    ));
}
