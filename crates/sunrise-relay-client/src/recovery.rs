//! The relay routes `docs/03-crypto/recovery.md` §Recovery flow reads, and
//! the refusals a recovery branches on.
//!
//! `sunrise_client_core::recovery` runs the flow; it reaches the relay through
//! its `RecoveryRelay` trait, which it implements over this type. The
//! dependency points that way on purpose: this crate is the REST client and
//! names no engine, so the flow that opens a vault sits above it rather than
//! below. `sunrise recover` and the `UniFFI` seam's `recover_account` both
//! go through here, so the two clients read the same routes and classify the
//! same refusals.

use crate::api::{self, Credential, SecretString};
use crate::bootstrap::{register_device, DeviceIdentity};

/// Why the relay did not give a recovery what it asked for.
///
/// Split where the caller does something different. A missing step-up is
/// fixed by signing in again, a missing blob by nothing on this device, and
/// everything else is a network or a relay to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayRefusal {
    /// `403`: the bearer carries no fresh sign-in, and the blob route requires
    /// one (`docs/06-server/auth.md` §The step-up in front of step 3).
    #[error("{0}")]
    StepUpRequired(String),
    /// `404`: the account has no recovery blob stored.
    #[error("{0}")]
    NoBlob(String),
    /// Any other refusal or transport failure, in the relay client's words.
    #[error("{0}")]
    Other(String),
}

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

impl RelayRecovery {
    /// The account record's `identity_signing_pub`, or `None` where the
    /// account never published one.
    ///
    /// # Errors
    /// [`RelayRefusal`], classified by route and status.
    pub async fn account_identity_key(&self) -> Result<Option<String>, RelayRefusal> {
        self.client
            .get_account(None)
            .await
            .map(|r| r.into_inner().identity_signing_pub)
            .map_err(|e| refusal(Route::Account, &e, status_of(&e)))
    }

    /// The sealed recovery blob, base64url as the relay serves it.
    ///
    /// # Errors
    /// [`RelayRefusal::StepUpRequired`] for a bearer without a fresh sign-in,
    /// [`RelayRefusal::NoBlob`] for an account that stored none.
    pub async fn recovery_blob(&self) -> Result<String, RelayRefusal> {
        self.client
            .get_recovery_blob(None)
            .await
            .map(|r| r.into_inner().recovery_blob)
            .map_err(|e| refusal(Route::Blob, &e, status_of(&e)))
    }

    /// Register a device on the account this bearer names, and return the id
    /// the relay minted for it.
    ///
    /// # Errors
    /// [`RelayRefusal::Other`] with what the generated client reported.
    pub async fn register_device(&self, device: DeviceIdentity) -> Result<String, RelayRefusal> {
        register_device(&self.base_url, &self.bearer, device)
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
