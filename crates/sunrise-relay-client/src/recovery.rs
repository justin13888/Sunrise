//! The relay, as `docs/03-crypto/recovery.md` §Recovery flow needs it.
//!
//! [`sunrise_onboarding::RecoveryRelay`] is the flow's view of the relay; this
//! is that view over the generated client. `sunrise recover` and the `UniFFI`
//! seam's `recover_account` both use it, so the two clients read the same
//! routes and classify the same refusals.

use sunrise_onboarding::{RecoveringDevice, RecoveryRelay, RelayRefusal};

use crate::api::{self, Credential, SecretString};
use crate::bootstrap::{register_device, DeviceIdentity};

/// The relay at one origin, under one bearer.
///
/// The bearer must carry a completed OIDC step-up: the blob route refuses an
/// ordinary session with `403 AUTH_STEP_UP_REQUIRED`, which this reports as
/// [`RelayRefusal::StepUpRequired`].
pub struct RelayRecovery {
    client: api::Client,
    base_url: String,
    bearer: String,
}

// Hand-written: the bearer is a credential that releases a recovery blob, and a
// derived `Debug` would print it.
impl std::fmt::Debug for RelayRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRecovery")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RelayRecovery {
    /// A recovery against the relay at `base_url` (the origin, no path).
    ///
    /// # Errors
    /// [`RelayRefusal::Other`] when `base_url` is not a usable origin.
    pub fn new(base_url: &str, bearer: &str) -> Result<Self, RelayRefusal> {
        let client = api::Client::new(base_url)
            .map_err(|e| RelayRefusal::Other(format!("not a relay origin: {e}")))?
            .with_credential(
                "AccountToken",
                Credential::Bearer(SecretString::from(bearer.to_owned())),
            );
        Ok(Self {
            client,
            base_url: base_url.to_owned(),
            bearer: bearer.to_owned(),
        })
    }
}

impl RecoveryRelay for RelayRecovery {
    async fn account_identity_key(&self) -> Result<Option<String>, RelayRefusal> {
        self.client
            .get_account(None)
            .await
            .map(|r| r.into_inner().identity_signing_pub)
            .map_err(|e| refusal(Route::Account, &e, status_of(&e)))
    }

    async fn recovery_blob(&self) -> Result<String, RelayRefusal> {
        self.client
            .get_recovery_blob(None)
            .await
            .map(|r| r.into_inner().recovery_blob)
            .map_err(|e| refusal(Route::Blob, &e, status_of(&e)))
    }

    async fn register_device(&self, device: RecoveringDevice) -> Result<String, RelayRefusal> {
        register_device(
            &self.base_url,
            &self.bearer,
            DeviceIdentity {
                device_pub_s: device.device_pub_s,
                device_pub_d: None,
                device_cert: Some(sunrise_onboarding::encode_recovery_blob(
                    &device.device_cert,
                )),
                vault_device_id: Some(sunrise_id::crockford::encode_bytes(&device.vault_device_id)),
                nickname: device.nickname,
                platform: device.platform,
                app_version: device.app_version,
            },
        )
        .await
        .map_err(|e| RelayRefusal::Other(e.to_string()))
    }
}

/// The HTTP status a failed call carried, where it carried one.
fn status_of<E>(e: &api::Error<E>) -> Option<u16> {
    match e {
        api::Error::Api(v) => Some(v.status().as_u16()),
        api::Error::UnexpectedStatus { status, .. } => Some(status.as_u16()),
        _ => None,
    }
}

/// The route a failed call went to. A status means different things on each.
#[derive(Debug, Clone, Copy)]
enum Route {
    /// `GET /accounts/me`.
    Account,
    /// `GET /accounts/me/recovery_blob`.
    Blob,
}

/// Classify a failure by its route and status, keeping the client's own words.
///
/// By status and not by message: the generated `Display` is the client's to
/// change, and a recovery that stopped recognising a `403` would tell a user
/// to retype a code that was never the problem.
///
/// By route as well: only the blob route asks for a step-up. A `403` from
/// `GET /accounts/me` means the relay knows no account for this sign-in and
/// does not accept new ones (`sunrise-server` `resolve_bearer`), and a fresh
/// sign-in to the same account would be refused the same way.
fn refusal(route: Route, e: &dyn std::fmt::Display, status: Option<u16>) -> RelayRefusal {
    match (route, status) {
        (Route::Blob, Some(403)) => {
            RelayRefusal::StepUpRequired(format!("could not fetch the recovery blob: {e}"))
        }
        (Route::Blob, Some(404)) => {
            RelayRefusal::NoBlob(format!("could not fetch the recovery blob: {e}"))
        }
        (Route::Blob, _) => RelayRefusal::Other(format!("could not fetch the recovery blob: {e}")),
        (Route::Account, Some(403)) => RelayRefusal::Other(format!(
            "the relay has no account for this sign-in and does not accept new ones; \
             sign in with the account the recovery code belongs to: {e}"
        )),
        (Route::Account, _) => RelayRefusal::Other(format!("could not read the account: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_classified_by_its_status() {
        assert!(matches!(
            refusal(Route::Blob, &"documented API error (403)", Some(403)),
            RelayRefusal::StepUpRequired(_)
        ));
        assert!(matches!(
            refusal(Route::Blob, &"x", Some(404)),
            RelayRefusal::NoBlob(_)
        ));
        assert!(matches!(
            refusal(Route::Blob, &"x", Some(500)),
            RelayRefusal::Other(_)
        ));
        assert!(matches!(
            refusal(Route::Blob, &"transport failed", None),
            RelayRefusal::Other(_)
        ));
    }

    /// A `403` from `GET /accounts/me` is the relay refusing an unknown
    /// subject with sign-up disabled. Calling it a step-up would send the user
    /// round a sign-in loop that can never succeed.
    #[test]
    fn only_the_blob_route_asks_for_a_step_up() {
        let unknown = refusal(Route::Account, &"documented API error (403)", Some(403));
        assert!(matches!(unknown, RelayRefusal::Other(_)), "{unknown:?}");
        assert!(unknown.to_string().contains("no account"), "{unknown}");
        assert!(matches!(
            refusal(Route::Account, &"x", Some(404)),
            RelayRefusal::Other(_)
        ));
    }

    #[test]
    fn the_bearer_never_reaches_debug() {
        let relay = RelayRecovery::new("http://127.0.0.1:9", "secret-bearer").expect("an origin");
        let shown = format!("{relay:?}");
        assert!(!shown.contains("secret-bearer"), "{shown}");
    }
}
