//! Push notification fanout interface (APNs / FCM / WebPush).
//!
//! v1 self-host: `LoggingProvider` echoes intents to the metrics
//! registry. Production binds `apns2` / `fcm` / `web-push` clients.
//! The trait is async so HTTP delivery doesn't block the relay loop.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Push platform tag.
///
/// `kynos::Schema` so the typed surface can take this directly rather than a
/// free string: the three platforms then appear in the OpenAPI document as an
/// enum, and an unknown one is rejected by the parse rather than stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, kynos::Schema)]
#[serde(rename_all = "lowercase")]
pub enum PushPlatform {
    /// Apple Push Notification service.
    Apns,
    /// Firebase Cloud Messaging.
    Fcm,
    /// Browser Web Push.
    WebPush,
}

/// One device's registration of a provider push token.
///
/// Distinct from [`crate::api::devices::PushRegistration`], which is the
/// request body `POST /api/v1/devices/push-tokens` accepts. That one is the
/// wire shape a client sends; this one is what a provider needs in order to
/// deliver. The two carried the same name until the duplication became a
/// reader's problem: `api::signed`'s module doc names `Signed<PushRegistration>`
/// unqualified, and only one of the two can be meant.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Push intent. The relay enqueues this when a peer device is offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushIntent {
    /// Receiver registration.
    pub registration: PushTokenRegistration,
    /// Wakeup payload (opaque to v1; clients re-fetch on wake).
    pub payload: String,
}

/// Push delivery error.
#[derive(Debug, Error)]
pub enum PushError {
    /// Provider returned non-success.
    #[error("push provider rejected: {0}")]
    Rejected(String),
    /// Network or auth failure.
    #[error("push transport: {0}")]
    Transport(String),
}

/// Push provider trait.
#[async_trait]
pub trait PushProvider: Send + Sync + std::fmt::Debug {
    /// Send one push intent.
    async fn send(&self, intent: &PushIntent) -> Result<(), PushError>;
}

/// Self-host provider that records a counter per intent and returns OK.
#[derive(Debug, Clone)]
pub struct LoggingProvider {
    metrics: crate::Metrics,
}

impl LoggingProvider {
    /// Construct.
    #[must_use]
    pub fn new(metrics: crate::Metrics) -> Self {
        Self { metrics }
    }
}

#[async_trait]
impl PushProvider for LoggingProvider {
    async fn send(&self, intent: &PushIntent) -> Result<(), PushError> {
        let metric = match intent.registration.platform {
            PushPlatform::Apns => "sunrise_push_apns_total",
            PushPlatform::Fcm => "sunrise_push_fcm_total",
            PushPlatform::WebPush => "sunrise_push_web_total",
        };
        self.metrics.incr(metric);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `device_id_hex` alias is a documented compatibility promise —
    /// "accepted so clients written against the pre-persistence shape keep
    /// parsing" — and nothing tested it, so deleting the attribute would have
    /// broken exactly those clients silently.
    #[test]
    fn the_pre_persistence_device_id_spelling_still_parses() {
        let legacy: PushTokenRegistration = serde_json::from_str(
            r#"{"device_id_hex":"0000000000000000000000000Z","platform":"apns","token":"t"}"#,
        )
        .expect("the alias must keep parsing");
        assert_eq!(legacy.device_id, "0000000000000000000000000Z");

        // The current spelling reaches the same field, so the alias is an
        // addition rather than a replacement.
        let current: PushTokenRegistration = serde_json::from_str(
            r#"{"device_id":"0000000000000000000000000Z","platform":"apns","token":"t"}"#,
        )
        .expect("the current spelling parses");
        assert_eq!(current.device_id, legacy.device_id);
    }

    #[tokio::test]
    async fn logging_provider_increments_counter() {
        let m = crate::Metrics::new();
        let p = LoggingProvider::new(m.clone());
        let intent = PushIntent {
            registration: PushTokenRegistration {
                device_id: "0000000000000000000000000Z".into(),
                platform: PushPlatform::Fcm,
                token: "abc".into(),
            },
            payload: String::new(),
        };
        p.send(&intent).await.unwrap();
        assert_eq!(m.get("sunrise_push_fcm_total"), 1);
    }
}
