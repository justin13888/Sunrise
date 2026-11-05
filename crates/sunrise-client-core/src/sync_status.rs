//! How sync health reads, and whether to start sync at all — as codes.
//!
//! Two decisions every client makes and the Apple app made in Swift
//! (`SyncPresentation.swift`, `SyncPlan.swift`): which badge a
//! [`SyncState`] and an outbox depth show, and whether a relay URL, a bearer
//! and a relay device id add up to a connection. Both return codes and data,
//! never sentences: the words are each surface's, the decision is not.
//!
//! # Degraded is not Synced
//!
//! [`SyncState::Degraded`] means the relay has reported a range of ops it can
//! no longer produce, so this device is **known** to be missing data that
//! re-subscribing cannot recover. It is the only state that is not about the
//! connection — the stream is healthy and ops flow — and a badge that renders
//! it as "Synced" re-creates the silent data loss issue #19 was filed for.
//! [`SyncBadge::known_incomplete`] is true for it and nothing else, and its
//! [`SyncTone`] is an alert while connected.

pub use sunrise_sync::SyncState;

use crate::relay_url::{parse_relay_url, RelayOrigin, RelayUrlIssue};

/// Which badge a sync snapshot shows. One per [`SyncState`], with `Live`
/// split by whether anything is waiting to send, plus [`Self::Checking`] for
/// before the first answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncBadgeKind {
    /// No snapshot yet: the first query has not answered. Must not read as
    /// healthy.
    Checking,
    /// Live, outbox empty: everything through.
    Synced,
    /// Live with an outbox: connected and still sending.
    Sending,
    /// Replaying ops from the other devices.
    CatchingUp,
    /// Not connected. The vault is complete as far as it knows and catches up
    /// on reconnect; a supported way to work, not a fault.
    Offline,
    /// Connected, and known to be missing ops the relay can no longer serve.
    ChangesMissing,
    /// The relay ended the session and the driver will not reconnect on its
    /// own: signing in again or reopening is the user's move.
    Stopped,
}

/// How loud a badge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncTone {
    /// Everything through.
    Ok,
    /// Work in progress; nothing wrong.
    Working,
    /// Nothing wrong, nothing happening.
    Idle,
    /// Needs attention.
    Alert,
}

/// What the sync badge shows, as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncBadge {
    /// Which badge.
    pub kind: SyncBadgeKind,
    /// How loud.
    pub tone: SyncTone,
    /// Ops waiting to send, for a "3 changes waiting" line. Carried for every
    /// kind; [`Self::shows_pending`] says whether the badge mentions it.
    pub pending: u32,
    /// Whether this device is known to be missing data it cannot recover.
    /// True for [`SyncBadgeKind::ChangesMissing`] only: being offline is not
    /// this.
    pub known_incomplete: bool,
}

impl SyncBadge {
    /// The badge before any snapshot has answered.
    pub const CHECKING: Self = Self {
        kind: SyncBadgeKind::Checking,
        tone: SyncTone::Idle,
        pending: 0,
        known_incomplete: false,
    };

    /// Whether the line under the badge is the pending count: `Sending`, and
    /// `Offline` with a backlog. Everywhere else it is the kind's own
    /// sentence, or nothing.
    #[must_use]
    pub const fn shows_pending(&self) -> bool {
        self.pending > 0 && matches!(self.kind, SyncBadgeKind::Sending | SyncBadgeKind::Offline)
    }
}

/// The badge for `state` with `outbox_pending` ops waiting.
#[must_use]
pub const fn sync_badge(state: SyncState, outbox_pending: u32) -> SyncBadge {
    let (kind, tone) = match state {
        SyncState::Live if outbox_pending == 0 => (SyncBadgeKind::Synced, SyncTone::Ok),
        SyncState::Live => (SyncBadgeKind::Sending, SyncTone::Working),
        SyncState::CatchingUp => (SyncBadgeKind::CatchingUp, SyncTone::Working),
        SyncState::Disconnected => (SyncBadgeKind::Offline, SyncTone::Idle),
        // Deliberately not a variant of Synced, and an alert while connected:
        // only another device that still holds the ops can close the gap.
        SyncState::Degraded => (SyncBadgeKind::ChangesMissing, SyncTone::Alert),
        // Not Offline: offline retries on its own and this does not. Not
        // known incomplete either — nothing was lost, it is not moving.
        SyncState::Stopped => (SyncBadgeKind::Stopped, SyncTone::Alert),
    };
    SyncBadge {
        kind,
        tone,
        pending: outbox_pending,
        known_incomplete: matches!(state, SyncState::Degraded),
    }
}

/// Why sync stays off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncOffReason {
    /// No relay URL is set: the vault is local, and complete on this device.
    NoRelay,
    /// A relay URL is set and is not one ([`RelayUrlIssue`] says how; a
    /// `ws://` URL is [`RelayUrlIssue::WebSocket`], from before ADR-0023).
    BadRelayUrl(RelayUrlIssue),
    /// The platform's secure store was reached and would not read this
    /// device's relay id (#284). Not "never registered": the id may exist,
    /// and an unbound driver would either drop the ADR-0022 binding or be
    /// refused as a bad signature for a condition an unlock fixes.
    DeviceIdUnreadable,
}

/// Whether to start the sync driver, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlan {
    /// Stay local.
    Off(SyncOffReason),
    /// Reach `relay`, presenting `bearer` on every request and binding each to
    /// `relay_device_id` when there is one.
    Connect {
        /// The relay origin.
        relay: RelayOrigin,
        /// The bearer, or `None` for a self-host relay that takes none.
        bearer: Option<String>,
        /// The id the relay minted at registration, or `None` for a device
        /// that has not registered.
        relay_device_id: Option<String>,
    },
}

/// What the platform's store answered for this device's relay id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayDeviceIdRead {
    /// An id, or `None` for a device that never registered.
    Read(Option<String>),
    /// The store was reached and refused.
    Refused,
}

/// Plan sync from a relay URL, an access token and the relay device id.
///
/// * No relay URL is [`SyncOffReason::NoRelay`]; one that is not a relay
///   origin is [`SyncOffReason::BadRelayUrl`]. A relay reason outranks a
///   refused id read: it is the one the user can act on first.
/// * An empty or whitespace bearer is no bearer. Passing it through would be
///   refused as a malformed bearer rather than taken as the anonymous request
///   a self-host relay accepts.
/// * An empty relay device id is no binding, for the same reason one layer
///   down: an `X-Sunrise-Device` naming no row is answered as a bad bearer.
/// * No relay device id still connects. The id exists only after
///   registration, which happens over this same relay, and every self-host
///   deployment runs without one.
#[must_use]
pub fn plan_sync(
    relay_url: &str,
    access_token: Option<&str>,
    relay_device_id: &RelayDeviceIdRead,
) -> SyncPlan {
    let relay = match parse_relay_url(relay_url) {
        Ok(origin) => origin,
        Err(RelayUrlIssue::Empty) => return SyncPlan::Off(SyncOffReason::NoRelay),
        Err(issue) => return SyncPlan::Off(SyncOffReason::BadRelayUrl(issue)),
    };
    let relay_device_id = match relay_device_id {
        RelayDeviceIdRead::Refused => return SyncPlan::Off(SyncOffReason::DeviceIdUnreadable),
        RelayDeviceIdRead::Read(id) => non_blank(id.as_deref()),
    };
    SyncPlan::Connect {
        relay,
        bearer: non_blank(access_token),
        relay_device_id,
    }
}

fn non_blank(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [SyncState; 5] = [
        SyncState::Disconnected,
        SyncState::CatchingUp,
        SyncState::Live,
        SyncState::Degraded,
        SyncState::Stopped,
    ];

    /// The falsifier for the module: Degraded folded into the connected and
    /// fine case fails here.
    #[test]
    fn degraded_never_reads_as_synced() {
        let degraded = sync_badge(SyncState::Degraded, 0);
        assert_ne!(degraded.kind, sync_badge(SyncState::Live, 0).kind);
        assert_eq!(degraded.tone, SyncTone::Alert);
        assert!(degraded.known_incomplete);
    }

    /// Five states, five badges.
    #[test]
    fn every_state_is_distinguishable() {
        let kinds: std::collections::HashSet<_> =
            ALL.iter().map(|s| sync_badge(*s, 0).kind).collect();
        assert_eq!(kinds.len(), ALL.len());
    }

    #[test]
    fn only_degraded_is_known_incomplete() {
        for state in ALL {
            assert_eq!(
                sync_badge(state, 7).known_incomplete,
                state == SyncState::Degraded,
                "{state:?}"
            );
        }
        let checking = SyncBadge::CHECKING;
        assert!(!checking.known_incomplete);
    }

    /// A stopped driver will not retry, so it must not read as Offline.
    #[test]
    fn stopped_asks_the_user_to_act() {
        let stopped = sync_badge(SyncState::Stopped, 0);
        assert_ne!(stopped.kind, sync_badge(SyncState::Disconnected, 0).kind);
        assert_eq!(stopped.tone, SyncTone::Alert);
    }

    #[test]
    fn a_backlog_is_visible_while_connected() {
        let idle = sync_badge(SyncState::Live, 0);
        assert_eq!(
            (idle.kind, idle.tone),
            (SyncBadgeKind::Synced, SyncTone::Ok)
        );
        assert!(!idle.shows_pending());
        let busy = sync_badge(SyncState::Live, 3);
        assert_eq!(
            (busy.kind, busy.tone),
            (SyncBadgeKind::Sending, SyncTone::Working)
        );
        assert!(busy.shows_pending());
        assert_eq!(busy.pending, 3);
    }

    #[test]
    fn offline_is_idle_and_shows_its_backlog() {
        let offline = sync_badge(SyncState::Disconnected, 0);
        assert_eq!(offline.tone, SyncTone::Idle);
        assert!(!offline.shows_pending());
        assert!(sync_badge(SyncState::Disconnected, 1).shows_pending());
        // A backlog under any other badge is not what its line says.
        assert!(!sync_badge(SyncState::Degraded, 4).shows_pending());
        assert!(!sync_badge(SyncState::CatchingUp, 4).shows_pending());
    }

    #[test]
    fn an_unanswered_query_is_not_silence() {
        assert_eq!(SyncBadge::CHECKING.kind, SyncBadgeKind::Checking);
        assert_ne!(
            SyncBadge::CHECKING.kind,
            sync_badge(SyncState::Live, 0).kind
        );
    }

    fn connect(url: &str, bearer: Option<&str>, device: Option<&str>) -> SyncPlan {
        SyncPlan::Connect {
            relay: parse_relay_url(url).unwrap(),
            bearer: bearer.map(str::to_owned),
            relay_device_id: device.map(str::to_owned),
        }
    }

    fn read(id: Option<&str>) -> RelayDeviceIdRead {
        RelayDeviceIdRead::Read(id.map(str::to_owned))
    }

    #[test]
    fn no_relay_means_local_only() {
        for url in ["", "   "] {
            assert_eq!(
                plan_sync(url, Some("t"), &read(None)),
                SyncPlan::Off(SyncOffReason::NoRelay)
            );
        }
    }

    /// A self-host relay accepts a request with no bearer.
    #[test]
    fn a_self_host_relay_connects_with_no_bearer() {
        assert_eq!(
            plan_sync("http://127.0.0.1:8443", None, &read(None)),
            connect("http://127.0.0.1:8443", None, None)
        );
    }

    #[test]
    fn an_empty_token_is_no_token() {
        let url = "https://relay.example";
        assert_eq!(
            plan_sync(url, Some("  "), &read(None)),
            connect(url, None, None)
        );
        assert_eq!(
            plan_sync(url, Some("tok"), &read(None)),
            connect(url, Some("tok"), None)
        );
    }

    #[test]
    fn a_registered_device_carries_its_relay_id_and_an_empty_one_is_none() {
        let url = "https://relay.example";
        let id = "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
        assert_eq!(
            plan_sync(url, Some("tok"), &read(Some(id))),
            connect(url, Some("tok"), Some(id))
        );
        assert_eq!(
            plan_sync(url, Some("tok"), &read(Some("  "))),
            connect(url, Some("tok"), None)
        );
    }

    /// #284: a refused read is not a device that never registered.
    #[test]
    fn a_refused_read_keeps_sync_off_and_a_relay_reason_outranks_it() {
        assert_eq!(
            plan_sync(
                "https://relay.example",
                Some("tok"),
                &RelayDeviceIdRead::Refused
            ),
            SyncPlan::Off(SyncOffReason::DeviceIdUnreadable)
        );
        assert_eq!(
            plan_sync("", Some("tok"), &RelayDeviceIdRead::Refused),
            SyncPlan::Off(SyncOffReason::NoRelay)
        );
    }

    /// The scheme that was the only valid one until ADR-0023 is now refused
    /// with its own code, which is what lets a client say what to change.
    #[test]
    fn a_web_socket_url_is_refused_with_its_own_code() {
        assert_eq!(
            plan_sync("wss://relay.example/sync", None, &read(None)),
            SyncPlan::Off(SyncOffReason::BadRelayUrl(RelayUrlIssue::WebSocket))
        );
    }

    /// Plain HTTP to another machine is refused now, where `SyncPlan.swift`
    /// connected: sync sends the bearer on every request.
    #[test]
    fn plain_http_to_another_machine_is_refused() {
        assert_eq!(
            plan_sync("http://relay.example", Some("tok"), &read(None)),
            SyncPlan::Off(SyncOffReason::BadRelayUrl(RelayUrlIssue::InsecureRemote))
        );
    }
}
