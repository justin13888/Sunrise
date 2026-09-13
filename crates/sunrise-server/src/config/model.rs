//! The configuration value, its defaults, and the rules it must satisfy.
//!
//! Pure: no filesystem, no environment, no argument list. That is what lets
//! every rule [`ServerConfig::validate`] enforces be tested as a value, and it
//! is the reason the refusals here are phrased against fields rather than
//! against a file — the same value is built programmatically by tests and by
//! `file`, and both are held to it.
//!
//! The defaults live beside the fields they fill so that reading one tells you
//! what an unset key does, and [`binds_loopback`] is here because "is this
//! listener local" is a question about the value and not about where it came
//! from.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration. Read from `sunrise.toml` (production) or built
/// programmatically (tests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// `host:port` to bind.
    pub bind: String,
    /// Server's published version string.
    pub server_app_v: String,
    /// Optional OIDC issuer URL. `None` leaves the self-host
    /// [`crate::NullVerifier`] in place.
    pub oidc_issuer: Option<String>,
    /// OIDC client id. Tokens must carry it in `aud`; see
    /// `docs/06-server/auth.md` §per-request-auth.
    pub oidc_client_id: Option<String>,
    /// Whether a first login with no matching account provisions one.
    ///
    /// Per `docs/06-server/auth.md`: with this false the server rejects
    /// unknown-account tokens with `403 AUTH_SIGNUP_DISABLED` even though the
    /// IdP issued them. It is a server-side guard, not a sign-up UX — the IdP
    /// still owns invite codes, allow-lists and captchas.
    ///
    /// Defaults to `true`, which is what makes a fresh self-host binary usable
    /// without a provisioning step. An operator who fronts a public IdP is
    /// expected to turn it off once their accounts exist.
    #[serde(default = "default_allow_signup")]
    pub allow_signup: bool,
    /// Whether `X-Sunrise-Device` + `X-Sunrise-Device-Sig` are mandatory on
    /// authenticated REST requests (`header_sig_v1`).
    ///
    /// Defaults to **on wherever an OIDC issuer is configured**, and off for
    /// the single-tenant self-host verifier, which has no devices to tell apart
    /// and for which [`ConfigError::DeviceSigWithoutOidc`] rejects the flag
    /// anyway. Set it explicitly to override either way.
    ///
    /// When a binding *is* present it is always verified, whatever this says —
    /// the flag governs whether absence is tolerated, never whether a bad
    /// signature is.
    #[serde(default)]
    pub require_device_sig: bool,
    /// How often a live `/sync` session re-checks that its device is still
    /// registered, in milliseconds.
    ///
    /// The bound on how long a revoked device keeps receiving fan-out on a
    /// socket it already holds. Exposed rather than hard-coded so a test can
    /// drive the revocation path without waiting the production interval.
    #[serde(default = "default_device_recheck_ms")]
    pub device_recheck_ms: u64,
    /// Clock skew tolerated on token `exp`/`nbf`, in seconds.
    #[serde(default = "default_token_leeway_secs")]
    pub token_leeway_secs: u64,
    /// JWKS cache lifetime used when the issuer publishes no `Cache-Control`.
    #[serde(default = "default_jwks_ttl_secs")]
    pub jwks_default_ttl_secs: u64,
    /// How recently the end user must have authenticated for
    /// `GET /api/v1/accounts/me/recovery_blob` to serve, in seconds.
    ///
    /// The one step-up setting that needs no knowledge of the deployment's
    /// IdP, which is why it is the one with a default: the client asks for a
    /// fresh login with `max_age`, and the token comes back with an
    /// `auth_time` this window is measured against. See
    /// [`crate::auth::step_up`] for what is being guarded and why an ordinary
    /// bearer is not enough.
    #[serde(default = "default_recovery_max_auth_age_secs")]
    pub recovery_max_auth_age_secs: u64,
    /// `acr` values accepted on that route. Empty accepts any.
    ///
    /// No default is possible: `acr` values are the IdP's vocabulary, and one
    /// this server invented would match nothing anywhere. An operator who
    /// knows their issuer writes theirs here and gets a stronger gate than
    /// freshness alone.
    #[serde(default)]
    pub recovery_acr_values: Vec<String>,
    /// `amr` values accepted on that route, satisfied by intersection. Empty
    /// accepts any.
    #[serde(default)]
    pub recovery_amr_values: Vec<String>,
    /// Self-host SQLite path (None = ephemeral in-memory, suitable for
    /// tests).
    pub sqlite_path: Option<PathBuf>,
    /// Self-host blob root (None = `<sqlite_dir>/blobs`).
    pub blob_root: Option<PathBuf>,
    /// Exact-match CORS allowlist for browser clients. Empty = no browser
    /// origin is permitted, which is the correct default for a relay whose
    /// only client today is native.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Maximum accepted request body, in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

/// 2 MiB: comfortably above the largest legitimate REST body (a device cert or
/// a blob-finalize manifest) and far below anything that would pressure memory.
pub(super) const fn default_max_body_bytes() -> usize {
    2 * 1024 * 1024
}

const fn default_allow_signup() -> bool {
    true
}

/// One minute, the usual allowance for unsynchronised consumer clocks.
pub(super) const fn default_token_leeway_secs() -> u64 {
    60
}

/// 30 s: short enough that revoking a device means something promptly, long
/// enough that an idle socket is not a per-second query against `devices`.
fn default_device_recheck_ms() -> u64 {
    30_000
}

/// Five minutes. Long enough that the JWKS is not refetched per request, short
/// enough that a key the issuer retires stops being accepted promptly.
pub(super) const fn default_jwks_ttl_secs() -> u64 {
    300
}

/// Five minutes: long enough to walk through an IdP's login and MFA prompt on
/// a phone, short enough that the fresh authentication is still the one the
/// person at the keyboard performed.
const fn default_recovery_max_auth_age_secs() -> u64 {
    300
}

/// Why a [`ServerConfig`] was rejected at startup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// `NullVerifier` maps every caller to one identity, so exposing it off
    /// loopback publishes one shared vault namespace to the network.
    #[error(
        "refusing to bind {bind}: the single-tenant self-host verifier is in use, which maps \
         every caller to the same account. Bind to loopback, or configure an OIDC issuer."
    )]
    SingleTenantOffLoopback {
        /// The offending bind address.
        bind: String,
    },
    /// A CORS entry that is not a usable origin.
    #[error(
        "allowed_origins entry {0:?} is not a scheme-qualified origin (e.g. https://app.example)"
    )]
    BadOrigin(String),
    /// Nonsensical body cap.
    #[error("max_body_bytes must be greater than zero")]
    ZeroBodyLimit,
    /// An OIDC issuer without the client id that tokens must be audienced to.
    #[error(
        "oidc_issuer is set but oidc_client_id is not: without it there is no `aud` to check, \
         and any token the issuer minted for any relying party would be accepted here"
    )]
    MissingClientId,
    /// An issuer URL we will not fetch metadata from.
    #[error(
        "oidc_issuer {0:?} must be an https:// URL: a JWKS fetched over plaintext is a JWKS an \
         on-path attacker can replace"
    )]
    InsecureIssuer(String),
    /// A step-up window no authentication can ever satisfy.
    #[error(
        "recovery_max_auth_age_secs is zero: no `auth_time` can be inside a window of no width, \
         so GET /api/v1/accounts/me/recovery_blob would refuse every caller forever"
    )]
    ZeroRecoveryAuthAge,
    /// Device signatures demanded with no way to tell devices apart.
    #[error(
        "require_device_sig is set while the single-tenant self-host verifier is in use: every \
         caller maps to one account, so a device signature binds nothing"
    )]
    DeviceSigWithoutOidc,
}

impl ServerConfig {
    /// Validate before serving.
    ///
    /// Startup is the only point where a misconfiguration is cheap to fix; a
    /// half-configured relay that boots and *then* leaks is the failure mode
    /// this exists to prevent. `single_tenant` says whether the process is
    /// running the self-host `NullVerifier`.
    pub fn validate(&self, single_tenant: bool) -> Result<(), ConfigError> {
        if single_tenant && !binds_loopback(&self.bind) {
            return Err(ConfigError::SingleTenantOffLoopback {
                bind: self.bind.clone(),
            });
        }
        for o in &self.allowed_origins {
            // Reject `*` explicitly: a wildcard origin paired with credentials
            // is the exact v0 mistake, and an exact-match list has no use for
            // one anyway.
            if o == "*" || !(o.starts_with("http://") || o.starts_with("https://")) {
                return Err(ConfigError::BadOrigin(o.clone()));
            }
        }
        if self.max_body_bytes == 0 {
            return Err(ConfigError::ZeroBodyLimit);
        }
        if let Some(issuer) = &self.oidc_issuer {
            if self.oidc_client_id.is_none() {
                return Err(ConfigError::MissingClientId);
            }
            if !issuer.starts_with("https://") {
                return Err(ConfigError::InsecureIssuer(issuer.clone()));
            }
        }
        if self.require_device_sig && single_tenant {
            return Err(ConfigError::DeviceSigWithoutOidc);
        }
        // A misconfiguration that boots and then refuses every recovery is
        // exactly the failure `validate` exists to catch at the only point it
        // is cheap to fix.
        if self.recovery_max_auth_age_secs == 0 {
            return Err(ConfigError::ZeroRecoveryAuthAge);
        }
        Ok(())
    }

    /// The step-up this deployment demands in front of the recovery blob.
    #[must_use]
    pub fn recovery_step_up(&self) -> crate::auth::step_up::StepUpPolicy {
        crate::auth::step_up::StepUpPolicy {
            max_auth_age_secs: self.recovery_max_auth_age_secs,
            acr_values: self.recovery_acr_values.clone(),
            amr_values: self.recovery_amr_values.clone(),
            leeway_secs: self.token_leeway_secs,
        }
    }
}

/// Whether `bind` keeps the listener on the local host only.
pub(crate) fn binds_loopback(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map_or(bind, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A hostname we cannot resolve at config time is not provably
        // loopback, so treat it as exposed. Failing closed is the only safe
        // direction here.
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8443".into(),
            server_app_v: env!("CARGO_PKG_VERSION").into(),
            oidc_issuer: None,
            oidc_client_id: None,
            allow_signup: default_allow_signup(),
            require_device_sig: false,
            device_recheck_ms: default_device_recheck_ms(),
            token_leeway_secs: default_token_leeway_secs(),
            jwks_default_ttl_secs: default_jwks_ttl_secs(),
            recovery_max_auth_age_secs: default_recovery_max_auth_age_secs(),
            recovery_acr_values: Vec::new(),
            recovery_amr_values: Vec::new(),
            sqlite_path: None,
            blob_root: None,
            allowed_origins: Vec::new(),
            max_body_bytes: default_max_body_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bind: &str) -> ServerConfig {
        ServerConfig {
            bind: bind.into(),
            ..Default::default()
        }
    }

    /// The rule that matters: a single-tenant verifier maps every caller to one
    /// account, so binding it to a routable address publishes one shared vault
    /// namespace to the network. Refusing at startup is the whole point — a
    /// relay that boots and *then* leaks is the failure this prevents.
    #[test]
    fn single_tenant_may_only_bind_loopback() {
        for ok in ["127.0.0.1:8443", "localhost:8443", "[::1]:8443"] {
            assert!(cfg(ok).validate(true).is_ok(), "{ok} should be allowed");
        }
        for bad in ["0.0.0.0:8443", "192.168.1.10:8443", "[::]:8443"] {
            assert!(
                matches!(
                    cfg(bad).validate(true),
                    Err(ConfigError::SingleTenantOffLoopback { .. })
                ),
                "{bad} must be refused while single-tenant"
            );
        }
    }

    /// A hostname we cannot prove is loopback must fail closed.
    #[test]
    fn unresolvable_host_is_treated_as_exposed() {
        assert!(cfg("relay.example.com:8443").validate(true).is_err());
    }

    /// With a real verifier the same binds are fine — the restriction is about
    /// tenancy, not about addresses.
    #[test]
    fn multi_tenant_may_bind_anywhere() {
        assert!(cfg("0.0.0.0:8443").validate(false).is_ok());
    }

    #[test]
    fn wildcard_origin_is_rejected() {
        let mut c = cfg("127.0.0.1:8443");
        c.allowed_origins = vec!["*".into()];
        assert!(
            matches!(c.validate(true), Err(ConfigError::BadOrigin(_))),
            "a wildcard origin is the v0 mistake and has no meaning in an exact-match list"
        );
    }

    #[test]
    fn origins_must_be_scheme_qualified() {
        let mut c = cfg("127.0.0.1:8443");
        c.allowed_origins = vec!["app.example.com".into()];
        assert!(matches!(c.validate(true), Err(ConfigError::BadOrigin(_))));
        c.allowed_origins = vec!["https://app.example.com".into()];
        assert!(c.validate(true).is_ok());
    }

    #[test]
    fn zero_body_limit_is_rejected() {
        let mut c = cfg("127.0.0.1:8443");
        c.max_body_bytes = 0;
        assert_eq!(c.validate(true), Err(ConfigError::ZeroBodyLimit));
    }

    /// An issuer with no client id means no `aud` to check, which means every
    /// token that issuer ever minted — for any relying party — verifies here.
    #[test]
    fn an_issuer_without_a_client_id_is_refused() {
        let mut c = cfg("0.0.0.0:8443");
        c.oidc_issuer = Some("https://idp.example".into());
        assert_eq!(c.validate(false), Err(ConfigError::MissingClientId));
        c.oidc_client_id = Some("sunrise".into());
        assert!(c.validate(false).is_ok());
    }

    #[test]
    fn a_plaintext_issuer_is_refused() {
        let mut c = cfg("0.0.0.0:8443");
        c.oidc_issuer = Some("http://idp.example".into());
        c.oidc_client_id = Some("sunrise".into());
        assert!(matches!(
            c.validate(false),
            Err(ConfigError::InsecureIssuer(_))
        ));
    }

    /// Demanding a device signature while every caller resolves to the same
    /// synthetic account is a setting that reads as security and provides
    /// none; refuse it rather than let an operator believe it did something.
    #[test]
    fn device_signatures_are_meaningless_under_the_self_host_verifier() {
        let mut c = cfg("127.0.0.1:8443");
        c.require_device_sig = true;
        assert_eq!(c.validate(true), Err(ConfigError::DeviceSigWithoutOidc));
        assert!(c.validate(false).is_ok());
    }

    /// A fresh self-host binary has to be able to provision its own first
    /// account, so the guard ships open.
    #[test]
    fn signup_is_allowed_by_default() {
        assert!(ServerConfig::default().allow_signup);
    }

    #[test]
    fn defaults_are_valid_and_safe() {
        let d = ServerConfig::default();
        assert!(d.validate(true).is_ok(), "defaults must boot in self-host");
        assert!(
            d.allowed_origins.is_empty(),
            "no browser origin may be permitted by default"
        );
    }
}
