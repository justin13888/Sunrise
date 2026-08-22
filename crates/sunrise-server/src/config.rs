//! Server config (TOML-backed).

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
    /// Optional OIDC issuer URL.
    pub oidc_issuer: Option<String>,
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
        Ok(())
    }
}

/// Whether `bind` keeps the listener on the local host only.
fn binds_loopback(bind: &str) -> bool {
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
