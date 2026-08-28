//! `sunrise login` / `logout` / `whoami`: obtain and persist an OIDC bearer.
//!
//! The relay verifies bearer tokens and, until this existed, the CLI had no way
//! to get one except `SUNRISE_SYNC_TOKEN` — which meant obtaining a token by
//! some other means entirely and pasting it in.
//!
//! The flow is `sunrise-auth`'s, driven to completion here: discover, open the
//! browser, wait on the loopback redirect, exchange, persist. Persistence is a
//! mode-0600 file in the vault directory ([`sunrise_auth::FileStore`]); see its
//! module docs for why that and not the OS keychain.

use std::path::Path;
use std::sync::Arc;

use sunrise_auth::{
    open_in_browser, CredentialStore, Credentials, FileStore, HttpClient, HttpsClient, LoginError,
    OidcClient, DEFAULT_REDIRECT_TIMEOUT,
};

/// Issuer URL for the OIDC provider.
pub const ENV_ISSUER: &str = "SUNRISE_OIDC_ISSUER";
/// Client id registered with that provider.
pub const ENV_CLIENT_ID: &str = "SUNRISE_OIDC_CLIENT_ID";

/// What `login` needs before it can start.
#[derive(Debug, Clone)]
pub struct LoginConfig {
    /// The issuer URL.
    pub issuer: String,
    /// The client id.
    pub client_id: String,
}

impl LoginConfig {
    /// Read both from the environment, reporting which one is missing rather
    /// than a generic failure — the two are configured in different places.
    pub fn from_env() -> Result<Self, String> {
        let issuer = std::env::var(ENV_ISSUER)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("{ENV_ISSUER} is not set"))?;
        let client_id = std::env::var(ENV_CLIENT_ID)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("{ENV_CLIENT_ID} is not set"))?;
        Ok(Self { issuer, client_id })
    }
}

/// The device id this login should be bound to.
///
/// The issuer is asked to stamp it into the token's `device_id` claim, and the
/// relay refuses a token whose claim names a different device — so binding it
/// here is what makes a stolen token useless on another machine.
#[must_use]
pub fn device_id_hex(core: &sunrise_core::Core) -> String {
    core.device_id().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Run the full login flow and persist the result.
///
/// Returns the credentials it stored.
pub async fn login(
    cfg: &LoginConfig,
    device_id: &str,
    store: &dyn CredentialStore,
    now_ms: u64,
    announce: &mut dyn FnMut(&str),
) -> Result<Credentials, LoginError> {
    let http = Arc::new(HttpsClient::new()) as Arc<dyn HttpClient>;
    let client = OidcClient::new(cfg.issuer.clone(), cfg.client_id.clone(), http);

    let metadata = client.discover().await?;
    let session = client.begin_login(&metadata, device_id).await?;

    announce(session.authorize_url());
    // A browser that will not open is not fatal: the URL was just printed, and
    // a user on a headless box can paste it somewhere that has one.
    if let Err(e) = open_in_browser(session.authorize_url()) {
        announce(&format!("(could not open a browser: {e})"));
    }

    let capture = session.wait_for_redirect(DEFAULT_REDIRECT_TIMEOUT).await?;
    let creds = client.exchange(&capture, now_ms).await?;
    store.save(&creds)?;
    Ok(creds)
}

/// Forget any stored login.
pub fn logout(store: &dyn CredentialStore) -> Result<(), LoginError> {
    store.clear()
}

/// A one-line description of the stored login's state, for `whoami`.
#[must_use]
pub fn status_line(store: &dyn CredentialStore, now_ms: u64) -> String {
    match store.load() {
        Ok(Some(c)) if c.is_expired_at(now_ms) => {
            "logged in, but the access token has expired — run `sunrise login`".into()
        }
        Ok(Some(c)) => {
            let secs = (c.expires_at_ms.saturating_sub(now_ms)) / 1000;
            format!("logged in; access token valid for another {secs}s")
        }
        Ok(None) => "not logged in".into(),
        Err(e) => format!("cannot read stored credentials: {e}"),
    }
}

/// The store the CLI uses, in the vault directory.
#[must_use]
pub fn store_for(vault_dir: &Path) -> FileStore {
    FileStore::in_dir(vault_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(expires_at_ms: u64) -> Credentials {
        let mut c = Credentials::new("access".into(), Some("r".into()), Some(3600), 0);
        c.expires_at_ms = expires_at_ms;
        c
    }

    #[test]
    fn status_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_for(dir.path());
        assert_eq!(status_line(&s, 0), "not logged in");
    }

    #[test]
    fn status_distinguishes_expired_from_valid() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_for(dir.path());
        s.save(&creds(10_000)).unwrap();
        assert!(status_line(&s, 1_000).contains("valid for another 9s"));
        assert!(
            status_line(&s, 20_000).contains("expired"),
            "an expired login must not read as logged in"
        );
    }

    #[test]
    fn logout_forgets_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_for(dir.path());
        s.save(&creds(10_000)).unwrap();
        logout(&s).unwrap();
        assert_eq!(status_line(&s, 0), "not logged in");
    }

    #[test]
    fn a_missing_env_var_names_itself() {
        // Both unset in this test process unless someone exported them.
        if std::env::var(ENV_ISSUER).is_ok() {
            return;
        }
        let e = LoginConfig::from_env().unwrap_err();
        assert!(e.contains(ENV_ISSUER), "{e}");
    }
}
