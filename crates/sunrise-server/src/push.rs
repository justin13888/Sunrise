//! Push notification fanout interface (APNs / FCM / WebPush).
//!
//! v1 self-host: `LoggingProvider` echoes intents to the metrics
//! registry. Production binds `apns2` / `fcm` / `web-push` clients.
//! The trait is async so HTTP delivery doesn't block the relay loop.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Push platform tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushPlatform {
    /// Apple Push Notification service.
    Apns,
    /// Firebase Cloud Messaging.
    Fcm,
    /// Browser Web Push.
    WebPush,
}

/// One push registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRegistration {
    /// Owning device id (16 raw bytes; hex-encoded).
    pub device_id_hex: String,
    /// Platform.
    pub platform: PushPlatform,
    /// Provider-specific token (APNs hex, FCM string, WebPush JSON-encoded).
    pub token: String,
}

/// Push intent. The relay enqueues this when a peer device is offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushIntent {
    /// Receiver registration.
    pub registration: PushRegistration,
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

    #[tokio::test]
    async fn logging_provider_increments_counter() {
        let m = crate::Metrics::new();
        let p = LoggingProvider::new(m.clone());
        let intent = PushIntent {
            registration: PushRegistration {
                device_id_hex: "00".repeat(16),
                platform: PushPlatform::Fcm,
                token: "abc".into(),
            },
            payload: String::new(),
        };
        p.send(&intent).await.unwrap();
        assert_eq!(m.get("sunrise_push_fcm_total"), 1);
    }
}
