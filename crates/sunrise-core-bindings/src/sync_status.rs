//! Sync health and the sync plan as codes, lowered onto UniFFI:
//! [`sunrise_client_core::sync_status`]. A badge is data — which badge, how
//! loud, how many waiting — and each surface words it.
//!
//! The plan carries a `Core` prefix ([`CoreSyncPlan`]) because the Apple app
//! already declares a `SyncPlan` of its own, which this replaces in a later
//! pass; the generated Swift must not shadow it first.

use sunrise_client_core::sync_status::{
    self, RelayDeviceIdRead, SyncBadge, SyncBadgeKind, SyncOffReason, SyncPlan, SyncTone,
};
use sunrise_sync::SyncState;

use sunrise_client_core::relay_url::RelayUrlIssue;

/// See [`sunrise_client_core::sync_status::SyncBadgeKind`].
#[uniffi::remote(Enum)]
pub enum SyncBadgeKind {
    Checking,
    Synced,
    Sending,
    CatchingUp,
    Offline,
    ChangesMissing,
    Stopped,
}

/// See [`sunrise_client_core::sync_status::SyncTone`].
#[uniffi::remote(Enum)]
pub enum SyncTone {
    Ok,
    Working,
    Idle,
    Alert,
}

/// See [`sunrise_client_core::sync_status::SyncBadge`].
#[uniffi::remote(Record)]
pub struct SyncBadge {
    pub kind: SyncBadgeKind,
    pub tone: SyncTone,
    pub pending: u32,
    pub known_incomplete: bool,
}

/// The sync badge for a snapshot's `state` and outbox depth. `Degraded` is
/// never a variant of synced: it is the only state known to be missing data.
#[uniffi::export]
#[must_use]
pub fn sync_badge(state: SyncState, outbox_pending: u32) -> SyncBadge {
    sync_status::sync_badge(state, outbox_pending)
}

/// The badge before the first snapshot answers: checking, never healthy.
#[uniffi::export]
#[must_use]
pub fn sync_badge_checking() -> SyncBadge {
    SyncBadge::CHECKING
}

/// Whether the badge's second line is the pending count.
#[uniffi::export]
#[must_use]
pub fn sync_badge_shows_pending(badge: SyncBadge) -> bool {
    badge.shows_pending()
}

/// See [`sunrise_client_core::sync_status::SyncOffReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CoreSyncOffReason {
    /// No relay URL is set.
    NoRelay,
    /// The relay URL is not one.
    BadRelayUrl {
        /// Why.
        issue: RelayUrlIssue,
    },
    /// The secure store would not read this device's relay id.
    DeviceIdUnreadable,
}

/// See [`sunrise_client_core::sync_status::SyncPlan`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreSyncPlan {
    /// Stay local.
    Off {
        /// Why.
        reason: CoreSyncOffReason,
    },
    /// Start the driver.
    Connect {
        /// The relay origin.
        relay: String,
        /// The bearer, or none for a self-host relay.
        bearer: Option<String>,
        /// The relay device id, or none for an unregistered device.
        relay_device_id: Option<String>,
    },
}

/// Whether to start sync, and with what. `relay_device_id_refused` is the
/// secure store reached and refusing (#284); it is not a device that never
/// registered, which is `relay_device_id: None`.
#[uniffi::export]
#[must_use]
pub fn plan_sync(
    relay_url: String,
    access_token: Option<String>,
    relay_device_id: Option<String>,
    relay_device_id_refused: bool,
) -> CoreSyncPlan {
    let read = if relay_device_id_refused {
        RelayDeviceIdRead::Refused
    } else {
        RelayDeviceIdRead::Read(relay_device_id)
    };
    match sync_status::plan_sync(&relay_url, access_token.as_deref(), &read) {
        SyncPlan::Off(reason) => CoreSyncPlan::Off {
            reason: match reason {
                SyncOffReason::NoRelay => CoreSyncOffReason::NoRelay,
                SyncOffReason::BadRelayUrl(issue) => CoreSyncOffReason::BadRelayUrl { issue },
                SyncOffReason::DeviceIdUnreadable => CoreSyncOffReason::DeviceIdUnreadable,
            },
        },
        SyncPlan::Connect {
            relay,
            bearer,
            relay_device_id,
        } => CoreSyncPlan::Connect {
            relay: relay.into_string(),
            bearer,
            relay_device_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_lower_the_client_layer_unchanged() {
        assert_eq!(
            sync_badge(SyncState::Degraded, 0).kind,
            SyncBadgeKind::ChangesMissing
        );
        assert!(sync_badge_shows_pending(sync_badge(SyncState::Live, 2)));
        assert_eq!(sync_badge_checking().kind, SyncBadgeKind::Checking);
        assert_eq!(
            plan_sync("wss://relay.example/sync".into(), None, None, false),
            CoreSyncPlan::Off {
                reason: CoreSyncOffReason::BadRelayUrl {
                    issue: RelayUrlIssue::WebSocket
                }
            }
        );
        assert_eq!(
            plan_sync("https://relay.example".into(), Some("t".into()), None, true),
            CoreSyncPlan::Off {
                reason: CoreSyncOffReason::DeviceIdUnreadable
            }
        );
    }
}
