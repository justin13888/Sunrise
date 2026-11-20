//! The `[storage]` table, and the backend it chooses (ADR-0062 §6).
//!
//! `backend = "sqlite"`, the default, is the single-binary relay: one database
//! file under `data_dir`, needing nothing else. `backend = "postgres"` is the
//! relay as several nodes over one shared database: metadata, the durable
//! relay log, sync sessions and rate state live there, and every node
//! connecting to it serves every account.
//!
//! The Postgres keys name files, never values: `postgres_url_file` holds the
//! connection URL, password included, for the reason ADR-0060 §2 gives for the
//! database key — a process's environment and a config file are readable by
//! more than the process. `[storage] encrypt` is SQLite's at-rest story and is
//! refused beside Postgres, whose at-rest encryption is the operator's. The
//! names earlier drafts used, `mode`, `postgres_url` and `s3_*`, stay unknown
//! keys and are refused with the rest.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::ServerConfig;

/// Which metadata backend the relay runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// One SQLite file: the single-binary relay.
    #[default]
    Sqlite,
    /// One shared Postgres database: the relay as several nodes.
    Postgres,
}

/// The backend `[storage]` chose, and the files that reach a Postgres one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// `[storage] backend`.
    #[serde(default)]
    pub kind: StorageBackend,
    /// `[storage] postgres_url_file`: a file holding one Postgres connection
    /// URL or `key=value` string, mode `0600`.
    #[serde(default)]
    pub postgres_url_file: Option<PathBuf>,
    /// `[storage] postgres_ca_file`: PEM certificates the server's TLS
    /// certificate must chain to. Set, every connection is TLS and verified
    /// against these alone; unset, connections are plaintext and a URL asking
    /// for `sslmode=require` is refused when the relay opens the database.
    #[serde(default)]
    pub postgres_ca_file: Option<PathBuf>,
}

/// A `[storage]` table that names a backend it does not configure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BackendError {
    /// Postgres with no URL file.
    #[error("[storage] backend = \"postgres\" needs postgres_url_file")]
    MissingUrlFile,
    /// A Postgres key beside the SQLite backend, which would read nothing.
    #[error("[storage] {0} applies to backend = \"postgres\" only")]
    PostgresKeyOnSqlite(&'static str),
    /// SQLCipher's switch beside Postgres.
    #[error(
        "[storage] encrypt applies to the SQLite backend only: encrypt a Postgres database \
         through Postgres's own at-rest mechanism"
    )]
    EncryptOnPostgres,
    /// Postgres with blobs in a private temporary directory, which every node
    /// would hold a different one of.
    #[error(
        "[storage] backend = \"postgres\" needs data_dir: blobs live under it, on a mount \
         every relay node shares"
    )]
    NoBlobRoot,
}

impl BackendConfig {
    /// Whether these keys describe a backend that can be opened, beside the
    /// SQLite encryption switch and whether a blob root is configured.
    pub fn validate(&self, encrypt: bool, has_blob_root: bool) -> Result<(), BackendError> {
        match self.kind {
            StorageBackend::Sqlite => {
                if self.postgres_url_file.is_some() {
                    return Err(BackendError::PostgresKeyOnSqlite("postgres_url_file"));
                }
                if self.postgres_ca_file.is_some() {
                    return Err(BackendError::PostgresKeyOnSqlite("postgres_ca_file"));
                }
            }
            StorageBackend::Postgres => {
                if self.postgres_url_file.is_none() {
                    return Err(BackendError::MissingUrlFile);
                }
                if encrypt {
                    return Err(BackendError::EncryptOnPostgres);
                }
                if !has_blob_root {
                    return Err(BackendError::NoBlobRoot);
                }
            }
        }
        Ok(())
    }
}

/// The `[storage]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageTable {
    /// Directory holding the SQLite database and the blob tree. Leaving it
    /// unset keeps the ephemeral in-memory store, which is for tests only.
    pub data_dir: Option<PathBuf>,
    /// How long a SQLite statement waits on another connection's lock, in
    /// milliseconds. Maps to [`ServerConfig::sqlite_busy_timeout_ms`].
    pub busy_timeout_ms: Option<u64>,
    /// [`ServerConfig::account_delete_grace_days`].
    pub account_delete_grace_days: Option<u64>,
    /// [`ServerConfig::gc_grace_days`].
    pub gc_grace_days: Option<u64>,
    /// [`ServerConfig::pending_upload_ttl_hours`].
    pub pending_upload_ttl_hours: Option<u64>,
    /// [`ServerConfig::maintenance_interval_secs`].
    pub maintenance_interval_secs: Option<u64>,
    /// [`ServerConfig::sqlite_encrypt`].
    pub encrypt: Option<bool>,
    /// [`ServerConfig::sqlite_key_file`].
    pub key_file: Option<PathBuf>,
    /// [`BackendConfig::kind`].
    pub backend: Option<StorageBackend>,
    /// [`BackendConfig::postgres_url_file`].
    pub postgres_url_file: Option<PathBuf>,
    /// [`BackendConfig::postgres_ca_file`].
    pub postgres_ca_file: Option<PathBuf>,
}

impl StorageTable {
    /// Overlay this table onto `base`.
    ///
    /// `data_dir` expands to both the SQLite path and the blob root, which is
    /// the shape the doc promises operators; the two `ServerConfig` fields
    /// stay separate because tests set them independently.
    #[must_use]
    pub fn apply(self, mut base: ServerConfig) -> ServerConfig {
        if let Some(dir) = self.data_dir {
            base.sqlite_path = Some(dir.join("sunrise.db"));
            base.blob_root = Some(dir.join("blobs"));
        }
        if let Some(v) = self.busy_timeout_ms {
            base.sqlite_busy_timeout_ms = v;
        }
        if let Some(v) = self.account_delete_grace_days {
            base.account_delete_grace_days = v;
        }
        if let Some(v) = self.gc_grace_days {
            base.gc_grace_days = v;
        }
        if let Some(v) = self.pending_upload_ttl_hours {
            base.pending_upload_ttl_hours = v;
        }
        if let Some(v) = self.maintenance_interval_secs {
            base.maintenance_interval_secs = v;
        }
        if let Some(v) = self.encrypt {
            base.sqlite_encrypt = v;
        }
        if let Some(v) = self.key_file {
            base.sqlite_key_file = Some(v);
        }
        if let Some(v) = self.backend {
            base.backend.kind = v;
        }
        if let Some(v) = self.postgres_url_file {
            base.backend.postgres_url_file = Some(v);
        }
        if let Some(v) = self.postgres_ca_file {
            base.backend.postgres_ca_file = Some(v);
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::super::FileConfig;
    use super::*;

    fn parsed(src: &str) -> Result<ServerConfig, String> {
        FileConfig::parse(src, "t.toml")
            .map(|f| f.apply(ServerConfig::default()))
            .map_err(|e| e.to_string())
    }

    /// SQLite is the default and needs nothing; Postgres is chosen by name and
    /// reads its URL from a file.
    #[test]
    fn the_backend_defaults_to_sqlite_and_postgres_is_named() {
        let cfg = parsed("").unwrap();
        assert_eq!(cfg.backend, BackendConfig::default());
        assert_eq!(cfg.backend.kind, StorageBackend::Sqlite);
        assert!(cfg.backend.validate(false, false).is_ok());

        let cfg = parsed(
            "[storage]\ndata_dir = \"/var/lib/sunrise\"\nbackend = \"postgres\"\n\
             postgres_url_file = \"/etc/sunrise/pg.url\"\npostgres_ca_file = \"/etc/sunrise/ca.pem\"",
        )
        .unwrap();
        assert_eq!(cfg.backend.kind, StorageBackend::Postgres);
        assert_eq!(
            cfg.backend.postgres_url_file.as_deref(),
            Some(std::path::Path::new("/etc/sunrise/pg.url"))
        );
        assert!(cfg
            .backend
            .validate(cfg.sqlite_encrypt, cfg.resolved_blob_root().is_some())
            .is_ok());
    }

    /// Each half-configured backend is refused, and so are the keys earlier
    /// drafts named.
    #[test]
    fn a_half_configured_backend_is_refused() {
        let postgres = BackendConfig {
            kind: StorageBackend::Postgres,
            postgres_url_file: Some("/u".into()),
            postgres_ca_file: None,
        };
        assert_eq!(
            BackendConfig {
                postgres_url_file: None,
                ..postgres.clone()
            }
            .validate(false, true),
            Err(BackendError::MissingUrlFile)
        );
        assert_eq!(
            postgres.validate(true, true),
            Err(BackendError::EncryptOnPostgres)
        );
        assert_eq!(
            postgres.validate(false, false),
            Err(BackendError::NoBlobRoot)
        );
        assert_eq!(
            BackendConfig {
                kind: StorageBackend::Sqlite,
                ..postgres
            }
            .validate(false, true),
            Err(BackendError::PostgresKeyOnSqlite("postgres_url_file"))
        );
        for refused in [
            "mode = \"managed\"",
            "postgres_url = \"postgres://x\"",
            "s3_bucket = \"b\"",
            "backend = \"mysql\"",
        ] {
            assert!(
                parsed(&format!("[storage]\n{refused}")).is_err(),
                "{refused} must be refused"
            );
        }
    }

    /// Opening refuses a backend the keys refuse before reading the URL file,
    /// so neither the server nor the admin CLI, which both open through
    /// `ServerState::open`, connects to or migrates a database it should not.
    #[tokio::test]
    async fn opening_refuses_the_backend_before_touching_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = parsed(&format!(
            "[storage]\ndata_dir = \"{}\"\nbackend = \"postgres\"\n\
             postgres_url_file = \"{}\"\nencrypt = true",
            dir.path().display(),
            dir.path().join("absent.url").display(),
        ))
        .unwrap();
        let err = crate::state::ServerState::open(cfg.clone())
            .await
            .expect_err("encrypt beside Postgres is refused");
        assert!(
            matches!(
                err,
                crate::state::StartError::Backend(BackendError::EncryptOnPostgres)
            ),
            "{err}"
        );
        cfg.sqlite_encrypt = false;
        cfg.blob_root = None;
        cfg.sqlite_path = None;
        let err = crate::state::ServerState::open(cfg)
            .await
            .expect_err("Postgres without data_dir is refused");
        assert!(
            matches!(
                err,
                crate::state::StartError::Backend(BackendError::NoBlobRoot)
            ),
            "{err}"
        );
    }
}
