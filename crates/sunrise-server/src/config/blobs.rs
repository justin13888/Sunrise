//! The `[blobs]` table, and where attachment ciphertext lives (ADR-0062 §6,
//! §7).
//!
//! `backend = "filesystem"`, the default, is the tree under
//! [`ServerConfig::blob_root`]. `backend = "s3"` is one bucket of an
//! S3-compatible object store, which every relay node reaches. The S3 keys
//! name the store and a credentials **file**, never a secret: ADR-0062 §6
//! keeps credentials out of the config and the environment for the reason
//! ADR-0060 §2 gives for the database key. The file's contents are
//! [`crate::blob::S3Credentials::from_file`]'s to read; this half is pure.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ServerConfig;

/// Which [`crate::blob::BlobBackend`] holds attachment ciphertext.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlobBackendKind {
    /// The tree under [`ServerConfig::blob_root`]: the single-binary relay.
    #[default]
    Filesystem,
    /// An S3-compatible object store: [`crate::blob::S3Blobs`].
    S3,
}

/// The `[blobs]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobsConfig {
    /// `[blobs] backend`: `"filesystem"`, the default, or `"s3"`.
    #[serde(default)]
    pub backend: BlobBackendKind,
    /// `[blobs] s3_endpoint`: `http://` or `https://`, a host and an optional
    /// port, and nothing after them. Requests address the bucket in the path.
    #[serde(default)]
    pub s3_endpoint: Option<String>,
    /// `[blobs] s3_bucket`: an existing bucket the relay owns every key under
    /// `blobs/` and `probes/` of.
    #[serde(default)]
    pub s3_bucket: Option<String>,
    /// `[blobs] s3_region`: the region requests are signed for. MinIO's
    /// default is `us-east-1`.
    #[serde(default)]
    pub s3_region: Option<String>,
    /// `[blobs] s3_credentials_file`: a TOML file holding `access_key_id` and
    /// `secret_access_key`, mode `0600`.
    #[serde(default)]
    pub s3_credentials_file: Option<PathBuf>,
    /// `[blobs] s3_ca_file`: PEM certificates the store's TLS certificate is
    /// verified against, in place of the public web roots: a self-hosted
    /// store behind a private CA. Optional; `https://` endpoints only.
    #[serde(default)]
    pub s3_ca_file: Option<PathBuf>,
}

/// A `[blobs]` table that names a backend it does not configure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlobsConfigError {
    /// An S3 backend missing one of its keys.
    #[error("[blobs] backend = \"s3\" needs {0}")]
    Missing(&'static str),
    /// An S3 key beside the filesystem backend, which would read nothing.
    #[error("[blobs] {0} applies to backend = \"s3\" only")]
    S3KeyOnFilesystem(&'static str),
    /// An endpoint that is not a scheme, a host and a port.
    #[error(
        "[blobs] s3_endpoint {0:?} must be an http:// or https:// URL naming a host and an \
         optional port, with no path, query or credentials"
    )]
    BadEndpoint(String),
    /// A bucket name no S3-compatible store would accept.
    #[error(
        "[blobs] s3_bucket {0:?} is not a bucket name: 3 to 63 lowercase letters, digits, dots \
         and hyphens, beginning and ending with a letter or digit"
    )]
    BadBucket(String),
    /// A CA file beside an `http://` endpoint, where no certificate is
    /// presented to verify.
    #[error("[blobs] s3_ca_file needs an https:// s3_endpoint")]
    CaFileOnPlaintext,
}

impl BlobsConfig {
    /// Whether these keys describe a backend that can be opened.
    ///
    /// # Errors
    /// Each [`BlobsConfigError`] the keys earn.
    pub fn validate(&self) -> Result<(), BlobsConfigError> {
        let keys: [(&'static str, bool); 4] = [
            ("s3_endpoint", self.s3_endpoint.is_some()),
            ("s3_bucket", self.s3_bucket.is_some()),
            ("s3_region", self.s3_region.is_some()),
            ("s3_credentials_file", self.s3_credentials_file.is_some()),
        ];
        match self.backend {
            BlobBackendKind::Filesystem => {
                if let Some((key, _)) = keys.iter().find(|(_, set)| *set) {
                    return Err(BlobsConfigError::S3KeyOnFilesystem(key));
                }
                if self.s3_ca_file.is_some() {
                    return Err(BlobsConfigError::S3KeyOnFilesystem("s3_ca_file"));
                }
            }
            BlobBackendKind::S3 => {
                if let Some((key, _)) = keys.iter().find(|(_, set)| !*set) {
                    return Err(BlobsConfigError::Missing(key));
                }
                if self
                    .s3_region
                    .as_deref()
                    .is_some_and(|r| r.trim().is_empty())
                {
                    return Err(BlobsConfigError::Missing("s3_region"));
                }
                let endpoint = self.s3_endpoint.as_deref().unwrap_or_default();
                if !is_endpoint(endpoint) {
                    return Err(BlobsConfigError::BadEndpoint(endpoint.to_owned()));
                }
                if self.s3_ca_file.is_some() && !endpoint.starts_with("https://") {
                    return Err(BlobsConfigError::CaFileOnPlaintext);
                }
                let bucket = self.s3_bucket.as_deref().unwrap_or_default();
                if !is_bucket(bucket) {
                    return Err(BlobsConfigError::BadBucket(bucket.to_owned()));
                }
            }
        }
        Ok(())
    }

    /// Whether blobs live where every relay node reaches them without a
    /// shared mount.
    #[must_use]
    pub fn is_object_store(&self) -> bool {
        self.backend == BlobBackendKind::S3
    }
}

/// `http://` or `https://`, then a non-empty host and port with nothing that
/// starts a path, query, fragment or userinfo; one trailing `/` is allowed.
fn is_endpoint(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    !authority.is_empty()
        && !authority
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@') || c.is_whitespace())
}

/// The S3 bucket naming rules a path-style request depends on.
fn is_bucket(s: &str) -> bool {
    let edge = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (3..=63).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && edge(s.as_bytes().first())
        && edge(s.as_bytes().last())
}

impl ServerConfig {
    /// The blob root this config names: [`ServerConfig::blob_root`], else
    /// `blobs` beside the database file. `None` for an in-memory store with no
    /// root set, which has no directory of its own to put blobs beside; the
    /// state then keeps a private temporary one.
    #[must_use]
    pub fn resolved_blob_root(&self) -> Option<PathBuf> {
        self.blob_root.clone().or_else(|| {
            self.sqlite_path
                .as_deref()
                .and_then(Path::parent)
                .map(|dir| dir.join("blobs"))
        })
    }

    /// Whether the operator named where blobs live: a `blob_root`, or an
    /// object store. A tree the server chose for itself, beside the database
    /// or in a private temporary directory, is never swept as orphans.
    #[must_use]
    pub fn blobs_operator_named(&self) -> bool {
        self.blob_root.is_some() || self.blobs.is_object_store()
    }
}

#[cfg(test)]
mod tests {
    use super::super::storage::{BackendConfig, StorageBackend};
    use super::super::ConfigError;
    use super::*;

    /// A named root wins; with none, blobs go beside the database file; an
    /// in-memory store with none resolves to no path at all, never to a
    /// fixed one every server on the machine would share.
    #[test]
    fn the_blob_root_is_the_named_one_else_beside_the_database() {
        let named = ServerConfig {
            sqlite_path: Some("/var/lib/sunrise/sunrise.db".into()),
            blob_root: Some("/srv/blobs".into()),
            ..Default::default()
        };
        assert_eq!(
            named.resolved_blob_root(),
            Some(PathBuf::from("/srv/blobs"))
        );

        let beside = ServerConfig {
            sqlite_path: Some("/var/lib/sunrise/sunrise.db".into()),
            ..Default::default()
        };
        assert_eq!(
            beside.resolved_blob_root(),
            Some(PathBuf::from("/var/lib/sunrise/blobs"))
        );

        assert_eq!(ServerConfig::default().resolved_blob_root(), None);
    }

    fn s3() -> BlobsConfig {
        BlobsConfig {
            backend: BlobBackendKind::S3,
            s3_endpoint: Some("https://s3.example.com".into()),
            s3_bucket: Some("sunrise-blobs".into()),
            s3_region: Some("us-east-1".into()),
            s3_credentials_file: Some("/etc/sunrise/s3.toml".into()),
            s3_ca_file: None,
        }
    }

    /// A CA file is optional, and only where there is a certificate to
    /// verify: an S3 backend on `https://`.
    #[test]
    fn a_ca_file_needs_an_https_object_store() {
        let ca = Some(PathBuf::from("/etc/sunrise/s3-ca.pem"));
        assert_eq!(
            BlobsConfig {
                s3_ca_file: ca.clone(),
                ..s3()
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            BlobsConfig {
                s3_endpoint: Some("http://127.0.0.1:9000".into()),
                s3_ca_file: ca.clone(),
                ..s3()
            }
            .validate(),
            Err(BlobsConfigError::CaFileOnPlaintext)
        );
        assert_eq!(
            BlobsConfig {
                s3_ca_file: ca,
                ..BlobsConfig::default()
            }
            .validate(),
            Err(BlobsConfigError::S3KeyOnFilesystem("s3_ca_file"))
        );
    }

    /// The filesystem is the default and takes no S3 key; S3 takes all four,
    /// each well-formed.
    #[test]
    fn the_blobs_table_takes_its_keys_whole_and_on_its_backend_only() {
        assert_eq!(BlobsConfig::default().validate(), Ok(()));
        assert_eq!(s3().validate(), Ok(()));
        assert_eq!(
            BlobsConfig {
                backend: BlobBackendKind::Filesystem,
                ..s3()
            }
            .validate(),
            Err(BlobsConfigError::S3KeyOnFilesystem("s3_endpoint"))
        );
        assert_eq!(
            BlobsConfig {
                s3_credentials_file: None,
                ..s3()
            }
            .validate(),
            Err(BlobsConfigError::Missing("s3_credentials_file"))
        );
        assert_eq!(
            BlobsConfig {
                s3_region: Some(" ".into()),
                ..s3()
            }
            .validate(),
            Err(BlobsConfigError::Missing("s3_region"))
        );
        for good in ["http://127.0.0.1:9000", "https://minio.internal:9000/"] {
            let cfg = BlobsConfig {
                s3_endpoint: Some(good.into()),
                ..s3()
            };
            assert_eq!(cfg.validate(), Ok(()), "{good}");
        }
        for bad in [
            "s3.example.com",
            "ftp://s3.example.com",
            "https://",
            "https://s3.example.com/bucket",
            "https://user:pw@s3.example.com",
            "https://s3.example.com?x=1",
        ] {
            let cfg = BlobsConfig {
                s3_endpoint: Some(bad.into()),
                ..s3()
            };
            assert_eq!(
                cfg.validate(),
                Err(BlobsConfigError::BadEndpoint(bad.into())),
                "{bad}"
            );
        }
        for bad in [
            "ab",
            "Upper",
            "-lead",
            "trail.",
            "under_score",
            &"a".repeat(64),
        ] {
            let cfg = BlobsConfig {
                s3_bucket: Some(bad.to_owned()),
                ..s3()
            };
            assert_eq!(
                cfg.validate(),
                Err(BlobsConfigError::BadBucket(bad.to_owned())),
                "{bad}"
            );
        }
    }

    /// An object store is already shared by every node, so Postgres needs no
    /// blob mount beside it, and the orphan sweep runs over it.
    #[test]
    fn an_object_store_stands_in_for_a_named_blob_root() {
        let mut cfg = ServerConfig {
            backend: BackendConfig {
                kind: StorageBackend::Postgres,
                postgres_url_file: Some("/etc/sunrise/pg.url".into()),
                postgres_ca_file: None,
            },
            ..Default::default()
        };
        assert!(cfg.validate_backend().is_err());
        assert!(!cfg.blobs_operator_named());
        cfg.blobs = s3();
        assert_eq!(cfg.validate_backend(), Ok(()));
        assert!(cfg.blobs_operator_named());
        cfg.blobs.s3_bucket = Some("X".into());
        assert_eq!(
            cfg.validate(false),
            Err(ConfigError::Blobs(BlobsConfigError::BadBucket("X".into())))
        );
    }
}
