//! Content-less wake-up pushes for devices with no open event stream.
//!
//! `docs/06-server/push-notifications.md` is the design. What runs:
//!
//! 1. `POST /sync/ops` appends a fresh batch and hands [`Dispatcher::notify`]
//!    a [`Wake`]: the account, the stream, and the device that sent it. That
//!    is one `try_send` into a bounded queue — no store read, no lock the
//!    append path holds, no await. A full queue drops the wake and counts it.
//! 2. One worker task drains the queue. For each wake it looks up the
//!    account's active devices holding a token for the provider's platform,
//!    skips the sender and every device with an event stream open
//!    ([`Presence`]), and asks the [`Planner`] whether this
//!    `(device, stream, kind)` already had a push inside the coalescing window
//!    and whether the device is under its per-minute cap.
//! 3. Each push that survives is delivered on its own task, at most
//!    [`Tuning::in_flight`] at once, with retries on throttling and server
//!    errors. A provider that says the token is dead gets the token deleted.
//!
//! The payload carries nothing: no stream id, no count, no text. The device
//! wakes, opens a session, and syncs.
//!
//! The token is the one secret here, and it is written in no log record and
//! no error message. [`PushTokenRegistration`]'s `Debug` redacts it for the
//! same reason.
//!
//! [`apns`] is the one provider. [`dispatch`] is everything between an append
//! and a provider call: presence, the queue, coalescing, the cap, retries.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod apns;
pub mod dispatch;

pub use apns::{ApnsProvider, PushSetupError, APNS_PAYLOAD};
pub use dispatch::{from_config, Dispatcher, Offer, Planner, Presence, Present, Tuning, Wake};

/// Push platform tag.
///
/// `kynos::Schema` so the typed surface can take this directly rather than a
/// free string: the three platforms then appear in the OpenAPI document as an
/// enum, and an unknown one is rejected by the parse rather than stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, kynos::Schema)]
#[serde(rename_all = "lowercase")]
pub enum PushPlatform {
    /// Apple Push Notification service.
    Apns,
    /// Firebase Cloud Messaging.
    Fcm,
    /// Browser Web Push.
    WebPush,
}

impl PushPlatform {
    /// The `push_tokens.platform` value a registration is stored under.
    #[must_use]
    pub const fn store_tag(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
            Self::WebPush => "webpush",
        }
    }

    /// The `provider` label value `docs/06-server/metrics.md` allows.
    #[must_use]
    pub const fn metric_label(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
            Self::WebPush => "web",
        }
    }
}

/// One device's registration of a provider push token.
///
/// Distinct from [`crate::api::devices::PushRegistration`], which is the
/// request body `POST /api/v1/devices/push-tokens` accepts. That one is the
/// wire shape a client sends; this one is what a provider needs in order to
/// deliver. The two carried the same name until the duplication became a
/// reader's problem: `api::signed`'s module doc names `Signed<PushRegistration>`
/// unqualified, and only one of the two can be meant.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushTokenRegistration {
    /// Owning device id — Crockford base-32 of 16 bytes, as issued by
    /// `POST /api/v1/devices`. The `device_id_hex` alias is accepted so
    /// clients written against the pre-persistence shape keep parsing.
    #[serde(alias = "device_id_hex")]
    pub device_id: String,
    /// Platform.
    pub platform: PushPlatform,
    /// Provider-specific token (APNs hex, FCM string, WebPush JSON-encoded).
    pub token: String,
}

/// Redacts the token: a `{:?}` anywhere a registration travels must not be
/// how a token reaches a log (`docs/06-server/observability.md` §What we
/// never log).
impl std::fmt::Debug for PushTokenRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushTokenRegistration")
            .field("device_id", &self.device_id)
            .field("platform", &self.platform)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Why a device is being woken. The third element of the coalescing key.
///
/// Only `sync` is sent. `reminder` and `mention` are in the design and have no
/// sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PushKind {
    /// A peer device published ops to a stream this device follows.
    Sync,
}

/// One push to deliver: a token, and why.
///
/// There is no payload field. Every provider sends a fixed, content-less body,
/// so nothing a caller puts here can reach a push provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    /// Receiver registration.
    pub registration: PushTokenRegistration,
    /// Why it is being woken.
    pub kind: PushKind,
}

/// Push delivery error.
///
/// Classified by what the dispatcher should do next, which is the only thing
/// it needs from a provider's status vocabulary. No variant's text may carry
/// the token.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PushError {
    /// The token will never be delivered to again: delete it.
    #[error("token unregistered: {0}")]
    Unregistered(String),
    /// The provider refused this push and would refuse it again.
    #[error("push provider rejected: {0}")]
    Rejected(String),
    /// The provider is throttling. Retried.
    #[error("push provider throttled: {0}")]
    Throttled(String),
    /// The provider, or the path to it, failed in a way a retry may clear.
    #[error("push transport: {0}")]
    Unavailable(String),
    /// No answer inside [`Tuning::send_timeout`]. Retried.
    #[error("push provider did not answer in time")]
    Timeout,
}

impl PushError {
    /// Whether another attempt could succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Throttled(_) | Self::Unavailable(_) | Self::Timeout
        )
    }

    /// The `result` label this outcome is counted under.
    #[must_use]
    pub const fn result(&self) -> &'static str {
        match self {
            Self::Unregistered(_) | Self::Rejected(_) => "rejected",
            Self::Throttled(_) => "rate_limited",
            Self::Unavailable(_) => "failed",
            Self::Timeout => "timeout",
        }
    }
}

/// Push provider trait.
#[async_trait]
pub trait PushProvider: Send + Sync + std::fmt::Debug {
    /// The platform whose tokens this provider delivers to.
    fn platform(&self) -> PushPlatform;

    /// Send one push intent.
    async fn send(&self, intent: &PushIntent) -> Result<(), PushError>;
}

#[cfg(test)]
mod tests;
