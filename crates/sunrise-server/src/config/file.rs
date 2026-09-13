//! Building a [`ServerConfig`] from the outside world.
//!
//! The TOML tables, the candidate paths, and the resolution order between an
//! explicit `-c`/`--config`, `$SUNRISE_CONFIG` and the implicit locations. This
//! is the only code in the crate that reads `std::env` or the config
//! filesystem, which is what keeps `model` testable as a value: everything
//! uncertain about where a config came from is decided here and handed over as
//! one already-overlaid [`ServerConfig`].
//!
//! Reading a file is separate from approving it. [`LoadError`] is about not
//! getting a value at all; [`ConfigError`](super::ConfigError) is about the
//! value that parsed.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::ServerConfig;

// `file_tests` asserts that an unset key keeps the model's default, and reads
// these through its `use super::*`. The loader itself names none of them: it
// overlays onto `ServerConfig::default`.
#[cfg(test)]
use super::model::{default_jwks_ttl_secs, default_max_body_bytes, default_token_leeway_secs};
#[cfg(test)]
use super::ConfigError;

/// Why a config file could not be turned into a [`ServerConfig`].
///
/// Separate from [`ConfigError`](super::ConfigError), which is about a config
/// that parsed but does not describe a safe server. This one is about not
/// getting that far.
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

    /// **Both directions of the one key whose default is computed rather than
    /// constant.** Unset means "decide from the deployment": a relay with a
    /// real issuer can tell devices apart, so binding is required there, and
    /// leaving it off would mean a stolen bearer alone is enough. Nothing
    /// asserted either direction, so collapsing the `match` to `false` — or to
    /// `true` — failed no test.
    #[test]
    fn an_unset_device_sig_flag_follows_whether_an_issuer_is_configured() {
        let with_issuer = FileConfig::parse(
            r#"
            [auth]
            oidc_issuer = "https://auth.example.com"
            oidc_client_id = "sunrise"
            "#,
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        assert!(
            with_issuer.require_device_sig,
            "a multi-tenant relay must demand the binding unless told otherwise"
        );

        let without_issuer = FileConfig::parse("[server]\nlisten = \"127.0.0.1:8080\"", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert!(
            !without_issuer.require_device_sig,
            "single-tenant self-host cannot tell devices apart, and `validate` \
             rejects the combination outright"
        );
    }

    /// An explicit setting wins over the computed default in both directions,
    /// which is the half a `match` arm ordering could silently lose.
    #[test]
    fn an_explicit_device_sig_flag_overrides_the_computed_default() {
        let off = FileConfig::parse(
            r#"
            [auth]
            oidc_issuer = "https://auth.example.com"
            oidc_client_id = "sunrise"
            require_device_sig = false
            "#,
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        assert!(!off.require_device_sig);

        let on = FileConfig::parse("[auth]\nrequire_device_sig = true", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert!(on.require_device_sig);
    }

    /// The one key whose file name differs from the field it lands on:
    /// `[auth] jwks_ttl_secs` sets `jwks_default_ttl_secs`, which is the TTL
    /// `auth::oidc` caches a JWKS document under when it carries no cache
    /// headers. Nothing asserted the mapping, so dropping the line — or
    /// pointing it at `token_leeway_secs` — failed nothing and an operator's
    /// setting would have been silently ignored.
    #[test]
    fn jwks_ttl_secs_lands_on_the_differently_named_field() {
        let cfg = FileConfig::parse("[auth]\njwks_ttl_secs = 900", "t.toml")
            .unwrap()
            .apply(ServerConfig::default());
        assert_eq!(cfg.jwks_default_ttl_secs, 900);
        assert_ne!(
            cfg.jwks_default_ttl_secs,
            default_jwks_ttl_secs(),
            "the test is worthless if it happens to be the default"
        );
        assert_eq!(
            cfg.token_leeway_secs,
            default_token_leeway_secs(),
            "and it must not have landed on a neighbouring field"
        );
    }

    /// `a_file_overlays_only_what_it_sets` asserts these two keep their
    /// defaults when unset, and nothing asserted they overlay when set — so
    /// dropping either assignment from `apply` failed no test.
    #[test]
    fn the_server_body_and_origin_limits_overlay_when_set() {
        let cfg = FileConfig::parse(
            r#"
            [server]
            max_body_bytes = 4096
            allowed_origins = ["https://app.example", "https://admin.example"]
            "#,
            "t.toml",
        )
        .unwrap()
        .apply(ServerConfig::default());
        assert_eq!(cfg.max_body_bytes, 4096);
        assert_ne!(cfg.max_body_bytes, default_max_body_bytes());
        assert_eq!(
            cfg.allowed_origins,
            vec![
                "https://app.example".to_string(),
                "https://admin.example".to_string()
            ]
        );
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
