//! What a mobile client runs when the OS wakes it in the background:
//! one bounded sync, and the push-token upload that makes the wake possible.
//!
//! `docs/07-clients/mobile-ios.md` §Background sync and §Push handling. iOS
//! hands an app a short, OS-chosen budget — an app refresh task or a silent
//! push — and suspends it again when the app reports done or the budget ends.
//! The long-lived driver [`SunriseCore::start_sync`] starts is the wrong shape
//! for that on its own: it never reports "done", and a session it holds from
//! before the app was suspended can look live while the socket under it is
//! long dead. [`SunriseCore::sync_once`] is the bounded shape.
//!
//! The mechanism — one driver, a fresh session proven by a dial count, and a
//! cancel that ends the wait — is [`sunrise_client_core::sync`], which this
//! module mirrors: [`SyncCancel`] wraps its cancel handle so the foreign side
//! can hold one, and [`SunriseCore::sync_once`] hands it the vault, the link
//! every transport this vault dials goes through, and a closure that starts
//! the driver the way [`SunriseCore::start_sync`] does.
//!
//! The handle is the only way an expiry reaches Rust: UniFFI's Swift async
//! glue has no cancellation path, so cancelling the Swift task alone would
//! leave this future waiting out its whole budget.

use std::sync::Arc;

use sunrise_client_core::sync::{self as shared, SyncStatusError};
use sunrise_sync::SyncState;

use crate::{BindingError, SunriseCore};

/// The kick and the dial count every transport this vault's driver dials
/// shares. See [`shared::SyncLink`].
pub(crate) type SyncLink = shared::SyncLink;

/// The foreign side's way to end a [`SunriseCore::sync_once`] early.
///
/// UniFFI's Swift async glue does not forward a Swift task's cancellation to
/// the Rust future it awaits, so an expiring iOS task that only cancelled its
/// own task would leave `sync_once` running to the end of its budget — past
/// the OS's deadline. The app makes one of these per run, cancels it from the
/// expiration handler, and `sync_once` returns as soon as it sees it.
///
/// One-way and sticky: a handle cancelled before `sync_once` is called makes
/// that call return at once, and a cancelled handle stays cancelled.
#[derive(Debug, uniffi::Object)]
pub struct SyncCancel {
    inner: shared::SyncCancel,
}

#[uniffi::export]
impl SyncCancel {
    /// A handle nobody has cancelled yet.
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: shared::SyncCancel::new(),
        })
    }

    /// End the `sync_once` this handle was passed to, or the next one.
    /// Idempotent, and sync so a non-async expiration handler can call it.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether [`Self::cancel`] has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

/// What one [`SunriseCore::sync_once`] achieved.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncOnceOutcome {
    /// A session dialled during this call caught up on every stream and
    /// emptied the outbox. `false` when the budget ran out first, or the
    /// relay refused in a way reconnecting will not fix.
    pub completed: bool,
    /// Changes the vault published while this call ran, inbound and local
    /// alike. Non-zero is what an iOS fetch handler reports as `.newData`.
    pub changes: u64,
    /// Local ops still waiting for the relay's ack when the call returned.
    pub outbox_pending: u32,
    /// The driver's state when the call returned.
    pub state: SyncState,
}

impl From<shared::SyncOnceReport> for SyncOnceOutcome {
    fn from(r: shared::SyncOnceReport) -> Self {
        Self {
            completed: r.completed,
            changes: r.changes,
            outbox_pending: r.outbox_pending,
            state: r.state,
        }
    }
}

impl From<SyncStatusError> for BindingError {
    fn from(e: SyncStatusError) -> Self {
        match e {
            SyncStatusError::Core(e) => Self::from(e),
            unexpected @ SyncStatusError::Unexpected(_) => Self::Core(unexpected.to_string()),
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Bring this vault level with the relay once, within `budget_ms`, and
    /// report what happened.
    ///
    /// The background-sync entry point (`docs/07-clients/mobile-ios.md`
    /// §Background sync, §Push handling). `url`, `bearer` and
    /// `relay_device_id` mean what they mean to [`Self::start_sync`], which
    /// this calls first: the driver is started if nothing has, and the bearer
    /// is replaced either way, so a token renewed just before the call is the
    /// one presented.
    ///
    /// Then every session dialled before the call is ended, and this waits for
    /// a session dialled after it to report `Live` with an empty outbox: every
    /// subscribed stream caught up, and every local op acked. A session from
    /// before the app was suspended does not count, however live it looks.
    ///
    /// Bounded by `budget_ms`, and idempotent: a second call with nothing new
    /// on either side completes with `changes == 0`. The driver keeps running
    /// when this returns, exactly as [`Self::start_sync`] left it; a
    /// suspended process simply stops scheduling it.
    ///
    /// Cancelling `cancel` — what an expired iOS task does — ends the wait at
    /// once with `completed == false`, and nothing else: the driver keeps
    /// what it applied, and no batch is half applied (see the module docs).
    /// A handle already cancelled when the call begins starts nothing.
    ///
    /// # Errors
    ///
    /// Only what [`Self::start_sync`] and a status read can raise — a closed
    /// vault, chiefly. An unreachable relay is not an error: it is
    /// `completed == false` with the state the driver reached.
    pub async fn sync_once(
        &self,
        url: String,
        bearer: Option<String>,
        relay_device_id: Option<String>,
        budget_ms: u64,
        cancel: Arc<SyncCancel>,
    ) -> Result<SyncOnceOutcome, BindingError> {
        shared::sync_once(
            &self.inner,
            &self.sync_link,
            budget_ms,
            &cancel.inner,
            || self.start_sync(url, bearer, relay_device_id),
        )
        .await
        .map(SyncOnceOutcome::from)
    }

    /// File this device's APNs token with the relay at `relay_url`, so the
    /// relay can wake it with a content-less push
    /// (`docs/06-server/push-notifications.md`).
    ///
    /// `relay_device_id` is the id [`Self::register_relay_device`] or
    /// [`Self::bootstrap_account`] returned: the request is signed as that
    /// device, and the relay refuses to file a token under a device the
    /// caller's account does not own. `token` is the APNs device token as
    /// lowercase hex.
    ///
    /// Idempotent at the relay — the row is keyed by device, so a repeat
    /// replaces it with itself and a rotated token replaces the old one.
    ///
    /// # Errors
    ///
    /// [`BindingError::Relay`] with what the relay or the request builder
    /// said.
    pub async fn register_push_token(
        &self,
        relay_url: String,
        bearer: String,
        relay_device_id: String,
        token: String,
    ) -> Result<(), BindingError> {
        let core = Arc::clone(&self.inner);
        sunrise_relay_client::register_push_token(
            relay_url.trim(),
            &bearer,
            sunrise_relay_client::PushTokenRegistration {
                relay_device_id,
                platform: sunrise_relay_client::api::types::PushPlatform::Apns,
                token,
            },
            self.inner.now_ms(),
            move |message| core.sign_with_device_key(message),
        )
        .await
        .map_err(|e| BindingError::Relay(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirror is the shared handle: cancelling it is what the shared
    /// wait sees.
    #[test]
    fn the_foreign_cancel_is_the_shared_one() {
        let cancel = SyncCancel::new();
        assert!(!cancel.is_cancelled() && !cancel.inner.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled() && cancel.inner.is_cancelled());
    }
}
