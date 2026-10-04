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
    /// How long a SQLite statement waits on another connection's lock before
    /// failing, in milliseconds. `0` fails at once. See
    /// [`crate::store::DEFAULT_BUSY_TIMEOUT`] for what it waits out.
    #[serde(default = "default_sqlite_busy_timeout_ms")]
    pub sqlite_busy_timeout_ms: u64,
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
    /// How long a shutdown waits for in-flight requests before cutting them,
    /// in seconds. [`ServerConfig::validate`] refuses `0`: kynos ends a
    /// zero-length drain as timed out without looking at what is in flight, so
    /// every stop would report work cut and exit 1.
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    /// The reverse proxies whose `Forwarded` / `X-Forwarded-For` may be
    /// believed, as addresses or CIDR networks.
    ///
    /// Empty — the default — believes nobody: the socket peer is the client,
    /// and every forwarding header is ignored, because a client can write
    /// those headers itself. With proxies listed, the client is the right-most
    /// address in the chain that is not one of them. The per-address rate
    /// limits key on that client, so behind a proxy this list is what keeps
    /// every user from sharing the proxy's one bucket.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// The `[limits]` table: every rate limit the relay enforces.
    #[serde(default)]
    pub limits: super::limits::LimitsConfig,
    /// The `[push]` table: the wake-up providers. Empty — the default — sends
    /// no push at all; see `docs/06-server/push-notifications.md`.
    #[serde(default)]
    pub push: PushConfig,
    /// Days between `DELETE /api/v1/accounts/me` and the maintenance pass
    /// that erases the account. `[storage] account_delete_grace_days`.
    #[serde(default = "default_thirty_days")]
    pub account_delete_grace_days: u64,
    /// Days a tombstoned blob is kept before it may be collected, quorum
    /// permitting. `[storage] gc_grace_days`, the name
    /// `docs/06-server/relay-and-blob-storage.md` §Retention gives it.
    #[serde(default = "default_thirty_days")]
    pub gc_grace_days: u64,
    /// Hours an upload may sit untouched between `init` and `finalize` before
    /// its pending chunks are swept. `[storage] pending_upload_ttl_hours`.
    #[serde(default = "default_pending_upload_ttl_hours")]
    pub pending_upload_ttl_hours: u64,
    /// Seconds between maintenance passes — account erasure, blob GC and the
    /// pending-upload sweep. `[storage] maintenance_interval_secs`.
    #[serde(default = "default_maintenance_interval_secs")]
    pub maintenance_interval_secs: u64,
}

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

/// Whether `s` is an Apple 10-character identifier (Key ID, Team ID).
fn is_apple_id(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// 2 MiB: comfortably above the largest legitimate REST body (a device cert or
/// a blob-finalize manifest) and far below anything that would pressure memory.
pub(super) const fn default_max_body_bytes() -> usize {
    2 * 1024 * 1024
}

/// 25 s: kynos's own default, which leaves a margin under the 30 s that
/// Docker, systemd and Kubernetes each wait between `SIGTERM` and `SIGKILL`.
pub(super) const fn default_shutdown_grace_secs() -> u64 {
    25
}

/// [`crate::store::DEFAULT_BUSY_TIMEOUT`], in the unit the config is written
/// in, so the two cannot disagree.
pub(super) fn default_sqlite_busy_timeout_ms() -> u64 {
    u64::try_from(crate::store::DEFAULT_BUSY_TIMEOUT.as_millis()).unwrap_or(u64::MAX)
}

/// Thirty days: the one window every retention number in
/// `docs/06-server/relay-and-blob-storage.md` §Retention is unified to.
pub(super) const fn default_thirty_days() -> u64 {
    30
}

/// A day: an upload abandoned that long is not coming back, and the client
/// that resumes one later simply starts a new `init`.
pub(super) const fn default_pending_upload_ttl_hours() -> u64 {
    24
}

/// An hour: every deadline the pass enforces is measured in days, so a pass an
/// hour late changes nothing a user can see.
pub(super) const fn default_maintenance_interval_secs() -> u64 {
    3600
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
    /// A drain deadline no drain can meet.
    #[error(
        "shutdown_grace_secs is zero: a drain of no length is always reported as timed out, so \
         every stop would log result = \"timed_out\" and exit 1 even with nothing in flight"
    )]
    ZeroShutdownGrace,
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
    /// A rate limit of zero, which refuses everything it covers.
    #[error(
        "[limits] {0} is zero, which refuses every request it covers; raise it, or set \
         `enabled = false` to turn the limits off"
    )]
    ZeroLimit(&'static str),
    /// A `trusted_proxies` entry that is not an address or a CIDR network.
    #[error(
        "trusted_proxies entry {0:?} is not an IP address or CIDR network (e.g. 10.0.0.0/8); \
         hostnames are not resolved, because a proxy's address is what the socket reports"
    )]
    BadTrustedProxy(String),
    /// A `[push.apns]` key that cannot be what Apple issued.
    #[error(
        "[push.apns] {0} is not a 10-character Apple identifier (uppercase letters and digits), \
         so every token minted with it would be refused by APNs"
    )]
    BadApnsId(&'static str),
    /// An empty `[push.apns] topic`.
    #[error("[push.apns] topic is empty; it must be the app's bundle id")]
    EmptyApnsTopic,
    /// A `[storage]` maintenance setting of zero that would destroy live work
    /// or spin.
    #[error(
        "[storage] {0} is zero: a pending-upload TTL of zero sweeps uploads still in flight, \
         and a maintenance interval of zero runs the pass in a busy loop"
    )]
    ZeroMaintenance(&'static str),
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
        if self.shutdown_grace_secs == 0 {
            return Err(ConfigError::ZeroShutdownGrace);
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
        if let Some(bad) = self
            .trusted_proxies
            .iter()
            .find(|entry| super::limits::parse_cidr(entry).is_none())
        {
            return Err(ConfigError::BadTrustedProxy(bad.clone()));
        }
        // Checked even with the limits off, so turning them back on cannot
        // be what reveals a value that never worked.
        if let Some(key) = self.limits.first_zero() {
            return Err(ConfigError::ZeroLimit(key));
        }
        // Both grace periods may be zero — collect as soon as the quorum
        // holds, erase on the next pass — but these two may not.
        if self.pending_upload_ttl_hours == 0 {
            return Err(ConfigError::ZeroMaintenance("pending_upload_ttl_hours"));
        }
        if self.maintenance_interval_secs == 0 {
            return Err(ConfigError::ZeroMaintenance("maintenance_interval_secs"));
        }
        // The key file itself is checked where it is read, in
        // `crate::push::from_config`: this half is pure.
        if let Some(apns) = &self.push.apns {
            if !is_apple_id(&apns.key_id) {
                return Err(ConfigError::BadApnsId("key_id"));
            }
            if !is_apple_id(&apns.team_id) {
                return Err(ConfigError::BadApnsId("team_id"));
            }
            if apns.topic.trim().is_empty() {
                return Err(ConfigError::EmptyApnsTopic);
            }
        }
        Ok(())
    }

    /// [`ServerConfig::trusted_proxies`] as the networks the router trusts.
    ///
    /// Entries that do not parse are skipped: [`ServerConfig::validate`]
    /// refuses them before a server starts, so only a config that skipped
    /// validation — a test — can reach this with one.
    #[must_use]
    pub fn trusted_proxy_networks(&self) -> Vec<(std::net::IpAddr, u8)> {
        self.trusted_proxies
            .iter()
            .filter_map(|entry| super::limits::parse_cidr(entry))
            .collect()
    }

    /// The maintenance pass's deadlines, in milliseconds.
    #[must_use]
    pub const fn retention(&self) -> Retention {
        const DAY_MS: u64 = 24 * 60 * 60 * 1000;
        Retention {
            account_delete_grace_ms: self.account_delete_grace_days.saturating_mul(DAY_MS),
            gc_grace_ms: self.gc_grace_days.saturating_mul(DAY_MS),
            pending_upload_ttl_ms: self.pending_upload_ttl_hours.saturating_mul(60 * 60 * 1000),
            // `attachments.md` §Deletion: a device silent for more than 30
            // days is abandoned and leaves the quorum. Not configurable: it is
            // the same window the relay log keeps ops for, so a device gone
            // longer must re-pair whatever this says.
            device_active_window_ms: 30 * DAY_MS,
        }
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

/// The deadlines [`ServerConfig::retention`] derives, in one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// From a deletion request to the account's erasure.
    pub account_delete_grace_ms: u64,
    /// From a blob's tombstone to the earliest it may be collected.
    pub gc_grace_ms: u64,
    /// How long an upload may go untouched before its chunks are swept.
    pub pending_upload_ttl_ms: u64,
    /// How recently a device must have declared a cursor to count toward a
    /// tombstone's quorum.
    pub device_active_window_ms: u64,
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
            sqlite_busy_timeout_ms: default_sqlite_busy_timeout_ms(),
            blob_root: None,
            account_delete_grace_days: default_thirty_days(),
            gc_grace_days: default_thirty_days(),
            pending_upload_ttl_hours: default_pending_upload_ttl_hours(),
            maintenance_interval_secs: default_maintenance_interval_secs(),
            allowed_origins: Vec::new(),
            max_body_bytes: default_max_body_bytes(),
            shutdown_grace_secs: default_shutdown_grace_secs(),
            trusted_proxies: Vec::new(),
            limits: super::limits::LimitsConfig::default(),
            push: PushConfig::default(),
        }
    }
}

impl ServerConfig {
    /// [`ServerConfig::shutdown_grace_secs`] as the `Duration` the server
    /// takes.
    #[must_use]
    pub const fn shutdown_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.shutdown_grace_secs)
    }

    /// [`ServerConfig::sqlite_busy_timeout_ms`] as the `Duration` the store
    /// takes.
    #[must_use]
    pub const fn sqlite_busy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.sqlite_busy_timeout_ms)
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

    /// kynos turns a zero drain deadline into `ShutdownTimeout` without
    /// waiting on anything, so `0` would make every stop exit 1.
    #[test]
    fn zero_shutdown_grace_is_rejected() {
        let mut c = cfg("127.0.0.1:8443");
        c.shutdown_grace_secs = 0;
        assert_eq!(c.validate(true), Err(ConfigError::ZeroShutdownGrace));
        c.shutdown_grace_secs = 1;
        assert!(c.validate(true).is_ok());
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

    /// A limit of zero refuses everything it covers, so it is refused at
    /// startup by name rather than discovered as a relay that serves nothing.
    #[test]
    fn a_zero_limit_is_refused_by_name() {
        let mut c = cfg("127.0.0.1:8443");
        c.limits.sessions_per_5min = 0;
        assert_eq!(
            c.validate(true),
            Err(ConfigError::ZeroLimit("sessions_per_5min"))
        );
        // Even with the limits off: turning them back on must not be what
        // reveals the value never worked.
        c.limits.enabled = false;
        assert!(c.validate(true).is_err());
    }

    /// A zero TTL would sweep uploads in flight and a zero interval would
    /// spin; zero grace periods are allowed and mean "as soon as possible".
    #[test]
    fn zero_maintenance_settings_are_refused_but_zero_grace_is_not() {
        let mut c = cfg("127.0.0.1:8443");
        c.gc_grace_days = 0;
        c.account_delete_grace_days = 0;
        assert_eq!(c.validate(true), Ok(()));
        c.pending_upload_ttl_hours = 0;
        assert_eq!(
            c.validate(true),
            Err(ConfigError::ZeroMaintenance("pending_upload_ttl_hours"))
        );
        c.pending_upload_ttl_hours = 1;
        c.maintenance_interval_secs = 0;
        assert_eq!(
            c.validate(true),
            Err(ConfigError::ZeroMaintenance("maintenance_interval_secs"))
        );
    }

    /// A proxy entry is an address or a network. A hostname would have to be
    /// resolved, and the address the socket reports is the only one that
    /// means anything here.
    #[test]
    fn trusted_proxies_must_be_addresses_or_networks() {
        let mut c = cfg("127.0.0.1:8443");
        c.trusted_proxies = vec!["127.0.0.1".into(), "10.0.0.0/8".into()];
        assert!(c.validate(true).is_ok());
        assert_eq!(c.trusted_proxy_networks().len(), 2);

        c.trusted_proxies.push("proxy.internal".into());
        assert_eq!(
            c.validate(true),
            Err(ConfigError::BadTrustedProxy("proxy.internal".into()))
        );
    }

    /// The safe default is to believe no forwarding header and to enforce
    /// the documented policy.
    #[test]
    fn by_default_no_proxy_is_trusted_and_the_limits_are_on() {
        let d = ServerConfig::default();
        assert!(d.trusted_proxies.is_empty());
        assert!(d.limits.enabled);
    }

    #[test]
    fn defaults_are_valid_and_safe() {
        let d = ServerConfig::default();
        assert!(d.validate(true).is_ok(), "defaults must boot in self-host");
        assert!(
            d.allowed_origins.is_empty(),
            "no browser origin may be permitted by default"
        );
        assert_eq!(d.push, PushConfig::default(), "no push provider by default");
    }

    /// An APNs table whose identifiers cannot be Apple's is refused by name:
    /// every JWT minted from it would be rejected, and the operator would see
    /// phones that stopped waking rather than this error.
    #[test]
    fn an_apns_table_with_malformed_identifiers_is_refused() {
        let apns = ApnsConfig {
            key_path: PathBuf::from("/etc/sunrise/AuthKey.p8"),
            key_id: "ABC123DEFG".into(),
            team_id: "DEF123GHIJ".into(),
            topic: "dev.sunrise.app".into(),
            environment: ApnsEnvironment::Production,
        };
        let mut c = cfg("127.0.0.1:8443");
        c.push.apns = Some(apns.clone());
        assert!(c.validate(true).is_ok());

        c.push.apns = Some(ApnsConfig {
            key_id: "abc123defg".into(),
            ..apns.clone()
        });
        assert_eq!(c.validate(true), Err(ConfigError::BadApnsId("key_id")));
        c.push.apns = Some(ApnsConfig {
            team_id: "SHORT".into(),
            ..apns.clone()
        });
        assert_eq!(c.validate(true), Err(ConfigError::BadApnsId("team_id")));
        c.push.apns = Some(ApnsConfig {
            topic: " ".into(),
            ..apns
        });
        assert_eq!(c.validate(true), Err(ConfigError::EmptyApnsTopic));
    }

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
