//! `[push]`: the wake-up providers.
//!
//! Its own module, like `limits` and `observability`, because it is one table
//! with its own value types. Pure, as `model` is: the signing key is read where
//! the provider is built, in `crate::push::from_config`, and the rules on these
//! values are enforced by [`ServerConfig::validate`](super::ServerConfig::validate).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `[push]` table.
///
/// One sub-table per provider. Only APNs exists; FCM and Web Push are designed
/// behind the same [`crate::push::PushProvider`] trait and not built.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    /// `[push.apns]`. `None` leaves iOS devices to wake on their own schedule.
    pub apns: Option<ApnsConfig>,
}

/// `[push.apns]`: token-based (`.p8`) authentication against APNs.
///
/// Every key is required. A partly written table is a refusal rather than a
/// provider that fails on its first send, which an operator would only notice
/// as phones that stopped syncing in the background.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApnsConfig {
    /// The `.p8` signing key Apple issued. Must not be readable by group or
    /// others; the server refuses to start otherwise.
    pub key_path: PathBuf,
    /// The key's 10-character Key ID, the JWT `kid`.
    pub key_id: String,
    /// The 10-character Apple Developer Team ID, the JWT `iss`.
    pub team_id: String,
    /// The app's bundle id, sent as `apns-topic`.
    pub topic: String,
    /// Which APNs gateway the tokens were issued against.
    pub environment: ApnsEnvironment,
}

/// The APNs gateway a device token belongs to.
///
/// A development build's token is valid only on the sandbox gateway and a
/// release build's only on production, so this is a property of the app build
/// the relay serves rather than a preference.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnvironment {
    /// `api.sandbox.push.apple.com`.
    Sandbox,
    /// `api.push.apple.com`.
    Production,
}

impl ApnsEnvironment {
    /// The gateway's origin.
    #[must_use]
    pub const fn endpoint(self) -> &'static str {
        match self {
            Self::Sandbox => "https://api.sandbox.push.apple.com",
            Self::Production => "https://api.push.apple.com",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_apns_environment_names_its_own_gateway() {
        assert_eq!(
            ApnsEnvironment::Sandbox.endpoint(),
            "https://api.sandbox.push.apple.com"
        );
        assert_eq!(
            ApnsEnvironment::Production.endpoint(),
            "https://api.push.apple.com"
        );
    }
}
