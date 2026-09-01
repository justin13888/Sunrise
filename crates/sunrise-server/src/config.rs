//! Server config (TOML-backed).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
const fn default_max_body_bytes() -> usize {
    2 * 1024 * 1024
}

const fn default_allow_signup() -> bool {
    true
}

/// One minute, the usual allowance for unsynchronised consumer clocks.
const fn default_token_leeway_secs() -> u64 {
    60
}

/// 30 s: short enough that revoking a device means something promptly, long
/// enough that an idle socket is not a per-second query against `devices`.
fn default_device_recheck_ms() -> u64 {
    30_000
}

/// Five minutes. Long enough that the JWKS is not refetched per request, short
/// enough that a key the issuer retires stops being accepted promptly.
const fn default_jwks_ttl_secs() -> u64 {
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
        Ok(())
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

// ---------------------------------------------------------------------------
// File-backed configuration
// ---------------------------------------------------------------------------

/// Why a config file could not be turned into a [`ServerConfig`].
///
/// Separate from [`ConfigError`], which is about a config that parsed but does
/// not describe a safe server. This one is about not getting that far.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// A path was named explicitly — by `--config` or `$SUNRISE_CONFIG` — and
    /// could not be read. Deliberately fatal: an operator who names a file is
    /// telling us the defaults are wrong, so falling back to them would run a
    /// server they did not ask for.
    #[error("cannot read config file {path}: {cause}")]
    Unreadable {
        /// The path that was named.
        path: String,
        /// The underlying I/O failure.
        cause: String,
    },
    /// The file was read but is not valid TOML, or does not match the schema.
    #[error("invalid config file {path}: {cause}")]
    Invalid {
        /// The file that failed to parse.
        path: String,
        /// The parser's complaint.
        cause: String,
    },
    /// An argument was passed that the server does not understand.
    #[error("unrecognized argument {arg:?}; usage: sunrise-server [-c|--config <path>]")]
    BadArgument {
        /// The offending argument.
        arg: String,
    },
    /// `--config` was passed with nothing after it.
    #[error("--config requires a path")]
    MissingConfigValue,
}

/// The `[server]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTable {
    /// `host:port` to bind. Maps to [`ServerConfig::bind`].
    pub listen: Option<String>,
    /// Browser origins allowed to call the REST API.
    pub allowed_origins: Option<Vec<String>>,
    /// Largest accepted request body, in bytes.
    pub max_body_bytes: Option<usize>,
}

/// The `[auth]` table.
///
/// Setting `oidc_issuer` **and** `oidc_client_id` is what installs
/// [`crate::OidcVerifier`]; with either missing the single-tenant
/// [`crate::NullVerifier`] stays in place and the server will refuse to bind
/// anything but loopback.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthTable {
    /// OIDC issuer URL. Must be `https://`.
    pub oidc_issuer: Option<String>,
    /// OIDC client id; tokens must carry it in `aud`.
    pub oidc_client_id: Option<String>,
    /// Whether an unknown subject may create an account.
    pub allow_signup: Option<bool>,
    /// Whether REST calls must carry `X-Sunrise-Device-Sig`.
    pub require_device_sig: Option<bool>,
    /// Clock-skew allowance when validating `exp`/`nbf`, in seconds.
    pub token_leeway_secs: Option<u64>,
    /// How long to cache a JWKS document that carries no cache headers.
    pub jwks_ttl_secs: Option<u64>,
}

/// The `[storage]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageTable {
    /// Directory holding the SQLite database and the blob tree. Leaving it
    /// unset keeps the ephemeral in-memory store, which is for tests only.
    pub data_dir: Option<PathBuf>,
}

/// A parsed `sunrise.toml`.
///
/// Every field is optional and overlays [`ServerConfig::default`], so a file
/// that sets one key changes one thing. Unknown keys and unknown tables are
/// **rejected** rather than ignored: silently dropping a `[tls]` block an
/// operator wrote would serve plaintext while they believed otherwise, and the
/// same reasoning applies to a misspelled key inside a table we do implement.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// The `[server]` table.
    #[serde(default)]
    pub server: ServerTable,
    /// The `[auth]` table.
    #[serde(default)]
    pub auth: AuthTable,
    /// The `[storage]` table.
    #[serde(default)]
    pub storage: StorageTable,
}

impl FileConfig {
    /// Parse a `sunrise.toml` body. `path` only labels errors.
    pub fn parse(src: &str, path: &str) -> Result<Self, LoadError> {
        toml::from_str(src).map_err(|e| LoadError::Invalid {
            path: path.to_string(),
            cause: e.to_string(),
        })
    }

    /// Overlay this file onto `base`, returning the effective config.
    ///
    /// `data_dir` expands to both the SQLite path and the blob root, which is
    /// the shape the doc promises operators; the two `ServerConfig` fields stay
    /// separate because tests set them independently.
    #[must_use]
    pub fn apply(self, mut base: ServerConfig) -> ServerConfig {
        if let Some(v) = self.server.listen {
            base.bind = v;
        }
        if let Some(v) = self.server.allowed_origins {
            base.allowed_origins = v;
        }
        if let Some(v) = self.server.max_body_bytes {
            base.max_body_bytes = v;
        }
        if let Some(v) = self.auth.oidc_issuer {
            base.oidc_issuer = Some(v);
        }
        if let Some(v) = self.auth.oidc_client_id {
            base.oidc_client_id = Some(v);
        }
        if let Some(v) = self.auth.allow_signup {
            base.allow_signup = v;
        }
        match self.auth.require_device_sig {
            Some(v) => base.require_device_sig = v,
            // Unset means "decide from the deployment", not "off". A relay with
            // a real issuer can tell devices apart, so binding is required
            // there by default: leaving it off would mean a stolen bearer alone
            // is enough, which is the property device binding exists to remove.
            // Single-tenant self-host cannot tell devices apart at all -- every
            // caller maps to one account -- and `validate` rejects the
            // combination outright, so it stays off there.
            //
            // Applied after `oidc_issuer` above, so it reads the issuer this
            // file actually resolved to rather than the default.
            None => base.require_device_sig = base.oidc_issuer.is_some(),
        }
        if let Some(v) = self.auth.token_leeway_secs {
            base.token_leeway_secs = v;
        }
        if let Some(v) = self.auth.jwks_ttl_secs {
            base.jwks_default_ttl_secs = v;
        }
        if let Some(dir) = self.storage.data_dir {
            base.sqlite_path = Some(dir.join("sunrise.db"));
            base.blob_root = Some(dir.join("blobs"));
        }
        base
    }
}

/// Default search path used when no config is named explicitly.
pub const IMPLICIT_CONFIG_PATHS: [&str; 2] = ["./sunrise.toml", "/etc/sunrise/sunrise.toml"];

/// Environment variable naming a config file.
pub const ENV_CONFIG: &str = "SUNRISE_CONFIG";

/// A config file location, and whether the operator asked for it by name.
#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Where to read from.
    pub path: PathBuf,
    /// True for `--config` and `$SUNRISE_CONFIG`. An explicit path that does
    /// not exist is fatal; an implicit one is simply not there.
    pub explicit: bool,
}

/// Pick the config file to read, in precedence order: `--config <path>` →
/// `$SUNRISE_CONFIG` → `./sunrise.toml` → `/etc/sunrise/sunrise.toml`.
///
/// `exists` decides whether an implicit candidate is present, injected so this
/// is testable without touching the filesystem. Returns `None` when nothing is
/// named and nothing is found — the defaults then apply, which is a loopback,
/// single-tenant server.
pub fn resolve_candidate(
    args: &[String],
    env_config: Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<Option<Candidate>, LoadError> {
    let mut explicit: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // Last `--config` wins, matching how every other CLI treats a
            // repeated flag.
            "-c" | "--config" => {
                let path = it.next().ok_or(LoadError::MissingConfigValue)?;
                explicit = Some(PathBuf::from(path));
            }
            other => {
                let rest =
                    other
                        .strip_prefix("--config=")
                        .ok_or_else(|| LoadError::BadArgument {
                            arg: other.to_string(),
                        })?;
                explicit = Some(PathBuf::from(rest));
            }
        }
    }
    if let Some(path) = explicit {
        return Ok(Some(Candidate {
            path,
            explicit: true,
        }));
    }
    if let Some(p) = env_config.filter(|p| !p.trim().is_empty()) {
        return Ok(Some(Candidate {
            path: PathBuf::from(p),
            explicit: true,
        }));
    }
    for p in IMPLICIT_CONFIG_PATHS {
        let path = PathBuf::from(p);
        if exists(&path) {
            return Ok(Some(Candidate {
                path,
                explicit: false,
            }));
        }
    }
    Ok(None)
}

/// Resolve, read, and apply the config file, over [`ServerConfig::default`].
///
/// Reads the process environment and filesystem; `args` is the argument list
/// with the program name already stripped.
pub fn load(args: &[String]) -> Result<ServerConfig, LoadError> {
    let candidate = resolve_candidate(args, std::env::var(ENV_CONFIG).ok(), &|p: &Path| {
        p.is_file()
    })?;
    let Some(candidate) = candidate else {
        return Ok(ServerConfig::default());
    };
    let display = candidate.path.display().to_string();
    let src = match std::fs::read_to_string(&candidate.path) {
        Ok(s) => s,
        // An implicit candidate that vanished between the check and the read
        // is treated as absent; a named one is fatal.
        Err(e) if !candidate.explicit && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServerConfig::default())
        }
        Err(e) => {
            return Err(LoadError::Unreadable {
                path: display,
                cause: e.to_string(),
            })
        }
    };
    Ok(FileConfig::parse(&src, &display)?.apply(ServerConfig::default()))
}

#[cfg(test)]
mod file_tests {
    use super::*;

    fn never(_: &Path) -> bool {
        false
    }

    #[test]
    fn a_file_overlays_only_what_it_sets() {
        let f = FileConfig::parse(
            r#"
            [server]
            listen = "0.0.0.0:443"
            [auth]
            oidc_issuer = "https://auth.example.com"
            oidc_client_id = "sunrise"
            allow_signup = false
            "#,
            "t.toml",
        )
        .unwrap();
        let cfg = f.apply(ServerConfig::default());
        assert_eq!(cfg.bind, "0.0.0.0:443");
        assert_eq!(cfg.oidc_issuer.as_deref(), Some("https://auth.example.com"));
        assert_eq!(cfg.oidc_client_id.as_deref(), Some("sunrise"));
        assert!(!cfg.allow_signup);
        // Untouched keys keep their defaults rather than being zeroed.
        assert_eq!(cfg.max_body_bytes, default_max_body_bytes());
        assert_eq!(cfg.token_leeway_secs, default_token_leeway_secs());
        assert!(cfg.allowed_origins.is_empty());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        let cfg = FileConfig::parse("", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert_eq!(cfg.bind, ServerConfig::default().bind);
        assert!(cfg.validate(true).is_ok(), "and still safe");
    }

    #[test]
    fn data_dir_expands_to_both_stores() {
        let cfg = FileConfig::parse("[storage]\ndata_dir = \"/var/lib/sunrise\"", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert_eq!(
            cfg.sqlite_path,
            Some(PathBuf::from("/var/lib/sunrise/sunrise.db"))
        );
        assert_eq!(cfg.blob_root, Some(PathBuf::from("/var/lib/sunrise/blobs")));
    }

    /// **The property the whole feature must not break.** A config file is now
    /// the way a public bind gets set, so it is also the way someone could set
    /// one without configuring an issuer — publishing a single shared account
    /// namespace to the network. Validation still refuses it.
    #[test]
    fn a_public_bind_without_an_issuer_is_still_refused() {
        let cfg = FileConfig::parse("[server]\nlisten = \"0.0.0.0:443\"", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert_eq!(
            cfg.validate(true),
            Err(ConfigError::SingleTenantOffLoopback {
                bind: "0.0.0.0:443".into()
            })
        );
        // And is accepted once an issuer makes it multi-tenant.
        let cfg = FileConfig::parse(
            r#"
            [server]
            listen = "0.0.0.0:443"
            [auth]
            oidc_issuer = "https://auth.example.com"
            oidc_client_id = "sunrise"
            "#,
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        assert!(cfg.validate(false).is_ok());
    }

    /// A table we do not implement must not be silently dropped: an operator
    /// who wrote `[tls]` believes the server is terminating TLS.
    #[test]
    fn an_unimplemented_table_is_rejected_not_ignored() {
        let e = FileConfig::parse("[tls]\nmode = \"acme\"", "t.toml").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("tls"), "{msg}");
    }

    #[test]
    fn a_misspelled_key_is_rejected() {
        let e = FileConfig::parse("[server]\nlisten_on = \"0.0.0.0:443\"", "t.toml").unwrap_err();
        assert!(e.to_string().contains("listen_on"), "{e}");
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let e = FileConfig::parse("[server", "sunrise.toml").unwrap_err();
        assert!(e.to_string().contains("sunrise.toml"), "{e}");
    }

    #[test]
    fn the_flag_wins_over_the_env_and_both_win_over_the_search_path() {
        let args = vec!["--config".to_string(), "/flag.toml".to_string()];
        let c = resolve_candidate(&args, Some("/env.toml".into()), &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(c.path, PathBuf::from("/flag.toml"));
        assert!(c.explicit);

        let c = resolve_candidate(&[], Some("/env.toml".into()), &|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(c.path, PathBuf::from("/env.toml"));
        assert!(c.explicit);

        let c = resolve_candidate(&[], None, &|_| true).unwrap().unwrap();
        assert_eq!(c.path, PathBuf::from(IMPLICIT_CONFIG_PATHS[0]));
        assert!(!c.explicit, "a found default is not an operator's request");
    }

    #[test]
    fn config_equals_form_is_accepted() {
        let args = vec!["--config=/a.toml".to_string()];
        let c = resolve_candidate(&args, None, &never).unwrap().unwrap();
        assert_eq!(c.path, PathBuf::from("/a.toml"));
    }

    #[test]
    fn the_search_path_is_tried_in_order() {
        let c = resolve_candidate(&[], None, &|p: &Path| {
            p == Path::new(IMPLICIT_CONFIG_PATHS[1])
        })
        .unwrap()
        .unwrap();
        assert_eq!(c.path, PathBuf::from(IMPLICIT_CONFIG_PATHS[1]));
    }

    /// Nothing named and nothing found is not an error. Making it one — as the
    /// doc used to promise — would break the zero-config loopback self-host
    /// boot that `defaults_are_valid_and_safe` protects.
    #[test]
    fn no_config_anywhere_falls_back_to_defaults() {
        assert_eq!(resolve_candidate(&[], None, &never).unwrap(), None);
        assert_eq!(
            resolve_candidate(&[], Some(String::new()), &never).unwrap(),
            None
        );
    }

    /// The scan covers every argument, not just the first. An earlier shape
    /// returned on the first one, so `--config a.toml --bogus` silently
    /// accepted the bogus flag.
    #[test]
    fn a_bad_argument_after_a_good_one_is_still_refused() {
        let args = vec![
            "--config".to_string(),
            "/a.toml".to_string(),
            "--bogus".to_string(),
        ];
        assert!(matches!(
            resolve_candidate(&args, None, &never),
            Err(LoadError::BadArgument { arg }) if arg == "--bogus"
        ));
    }

    #[test]
    fn the_last_config_flag_wins() {
        let args = vec![
            "--config".to_string(),
            "/a.toml".to_string(),
            "--config=/b.toml".to_string(),
        ];
        let c = resolve_candidate(&args, None, &never).unwrap().unwrap();
        assert_eq!(c.path, PathBuf::from("/b.toml"));
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        let args = vec!["--serve".to_string()];
        assert!(matches!(
            resolve_candidate(&args, None, &never),
            Err(LoadError::BadArgument { .. })
        ));
    }

    #[test]
    fn config_without_a_value_is_refused() {
        let args = vec!["--config".to_string()];
        assert!(matches!(
            resolve_candidate(&args, None, &never),
            Err(LoadError::MissingConfigValue)
        ));
    }

    /// A path the operator named and we cannot read is fatal, not a silent
    /// fallback to defaults: they told us the defaults are wrong.
    #[test]
    fn a_named_but_missing_file_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let args = vec!["--config".to_string(), missing.display().to_string()];
        assert!(matches!(load(&args), Err(LoadError::Unreadable { .. })));
    }

    #[test]
    fn a_named_file_is_read_and_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.toml");
        std::fs::write(&path, "[server]\nlisten = \"127.0.0.1:9999\"").unwrap();
        let args = vec!["--config".to_string(), path.display().to_string()];
        assert_eq!(load(&args).unwrap().bind, "127.0.0.1:9999");
    }
}
