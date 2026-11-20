//! The object store's key pair, read from `[blobs] s3_credentials_file`.

use std::path::{Path, PathBuf};

use crate::config::BlobsConfigError;

/// An access key pair. The secret never reaches `Debug` or a log line.
#[derive(Clone)]
pub struct S3Credentials {
    pub(super) access_key_id: String,
    pub(super) secret_access_key: zeroize::Zeroizing<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// The credentials file's shape.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsFile {
    access_key_id: String,
    secret_access_key: String,
}

impl S3Credentials {
    /// A pair given directly: an embedding's or a test's.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: zeroize::Zeroizing::new(secret_access_key.into()),
        }
    }

    /// Read `access_key_id` and `secret_access_key` from a TOML file only its
    /// owner may read, as `[storage] postgres_url_file` is.
    ///
    /// # Errors
    /// [`S3OpenError::CredentialsFile`] for a file that cannot be read or
    /// does not hold the two keys, [`S3OpenError::CredentialsPermissions`]
    /// for one group or others can read.
    pub fn from_file(path: &Path) -> Result<Self, S3OpenError> {
        let unreadable = |cause: String| S3OpenError::CredentialsFile {
            path: path.to_owned(),
            cause,
        };
        let meta = std::fs::metadata(path).map_err(|e| unreadable(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(S3OpenError::CredentialsPermissions {
                    path: path.to_owned(),
                    mode,
                });
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        let raw = zeroize::Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| unreadable(e.to_string()))?,
        );
        // The parser's message can quote the line it failed on, secret and
        // all, so it is not passed on.
        let file: CredentialsFile = toml::from_str(&raw).map_err(|_| {
            unreadable("not a TOML table of access_key_id and secret_access_key".to_owned())
        })?;
        let secret_access_key = zeroize::Zeroizing::new(file.secret_access_key);
        if file.access_key_id.trim().is_empty() || secret_access_key.trim().is_empty() {
            return Err(unreadable(
                "access_key_id or secret_access_key is empty".to_owned(),
            ));
        }
        Ok(Self {
            access_key_id: file.access_key_id,
            secret_access_key,
        })
    }
}

/// Why an [`super::S3Blobs`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum S3OpenError {
    /// The `[blobs]` keys do not describe a store.
    #[error(transparent)]
    Config(#[from] BlobsConfigError),
    /// The credentials file could not be read, or does not hold a key pair.
    #[error("[blobs] s3_credentials_file {}: {cause}", path.display())]
    CredentialsFile {
        /// The file named.
        path: PathBuf,
        /// What was wrong with it.
        cause: String,
    },
    /// The credentials file is readable by more than its owner.
    #[error(
        "[blobs] s3_credentials_file {} has mode {mode:o}: it holds the object store's secret \
         key, so only its owner may read it (chmod 600)",
        path.display()
    )]
    CredentialsPermissions {
        /// The file named.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner-only, both keys, and the secret in no error message.
    #[test]
    fn the_credentials_file_is_owner_only_and_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s3.toml");
        std::fs::write(
            &path,
            "access_key_id = \"AKIA\"\nsecret_access_key = \"hunter2\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                S3Credentials::from_file(&path),
                Err(S3OpenError::CredentialsPermissions { mode: 0o644, .. })
            ));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let creds = S3Credentials::from_file(&path).unwrap();
        assert_eq!(creds.access_key_id, "AKIA");
        assert!(!format!("{creds:?}").contains("hunter2"));

        std::fs::write(&path, "secret_access_key = \"hunter2\"\nextra = 1\n").unwrap();
        let err = S3Credentials::from_file(&path).unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(matches!(
            S3Credentials::from_file(&dir.path().join("absent")),
            Err(S3OpenError::CredentialsFile { .. })
        ));
    }
}
