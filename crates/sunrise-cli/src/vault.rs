//! Per-vault root keys: what makes `SUNRISE_VAULT` a second **account**
//! rather than a second directory.
//!
//! `docs/07-clients/parity-matrix.md` marks multi-account a CLI MUST and names
//! `SUNRISE_VAULT` as the mechanism. Pointing that variable somewhere else did
//! give each vault its own directory, its own device identity and its own
//! credential file — but every one of them was unlocked with a single
//! hardcoded 32-byte constant compiled into the binary. Two vaults keyed by
//! the same root are not isolated in any sense a user would recognise: the
//! SQLCipher key is a KDF of that root, so anyone holding this binary could
//! open either one, and both derive identical Stream keys, so an op envelope
//! from one decrypts under the other.
//!
//! # The model
//!
//! The macOS app solved this first, and this is the same shape with the
//! platform parts swapped:
//!
//! | | macOS | CLI |
//! |---|---|---|
//! | vault identity | `VaultRegistry` in `UserDefaults` | `vault-id` in the vault dir, listed in the keystore's `vaults` |
//! | where the root lives | login Keychain, account = vault id | `<keystore>/<id>.key`, mode 0600 |
//! | where the root comes from | `SecRandomCopyBytes` | the injected [`Rng`] |
//!
//! Each vault gets **its own root, generated once, at random**. There is no
//! passphrase derivation here for the same reason there is none on macOS: the
//! key is random, something local holds it, and a second device gets it by
//! pairing rather than by the user retyping anything.
//!
//! # Why the key is not in the vault directory
//!
//! Because then it would not be a key. The vault database is SQLCipher,
//! encrypted under a KDF of the root; a root file sitting beside it makes that
//! encryption decorative, and copying the directory — to a backup, to a shared
//! drive, to another machine — would copy the ability to read it. The Keychain
//! is not inside `Application Support` on macOS either.
//!
//! The cost is real and worth stating plainly: **backing up the vault
//! directory alone is not enough.** The keystore has to travel with it, or the
//! copy is ciphertext nobody can open. [`VaultError::RootMissing`] is the
//! error that says so.
//!
//! `FileStore`'s threat model applies here unchanged (`sunrise-auth::store`):
//! mode 0600 is the same protection the vault lock and the WAL already rely
//! on, and it is not sufficient against a local attacker who can read as this
//! user. It is, however, strictly better than a constant in the binary, which
//! is not protection against anyone at all.
//!
//! # Sharing a root on purpose
//!
//! Two vaults that *should* share a root — one account on two devices — is
//! pairing, and pairing over the wire is a later slice. [`ENV_VAULT_ROOT`] is
//! the stand-in, in exactly the spirit of `SUNRISE_PAIRING_FILE`: an
//! explicit, documented dev affordance that says "use this root", replacing an
//! implicit constant that said it for you. It is also the escape hatch for a
//! vault made before this existed — see [`VaultError::PreMultiAccount`].

use std::path::{Path, PathBuf};

use sunrise_core::Rng;

/// Env var: where per-vault root keys are stored. Default
/// `$XDG_DATA_HOME/sunrise/keys`, falling back to `~/.local/share/sunrise/keys`.
pub const ENV_KEYSTORE: &str = "SUNRISE_KEYSTORE";

/// Env var: use this 32-byte root (64 hex chars) and touch no keystore.
///
/// The pairing stand-in, and the way to open a vault whose key this machine
/// does not hold.
pub const ENV_VAULT_ROOT: &str = "SUNRISE_VAULT_ROOT";

/// The single root every vault was keyed by before this module existed, as
/// hex — quoted in [`VaultError::PreMultiAccount`] so the remedy for an old
/// vault is a command a user can paste, not an archaeology exercise.
pub const LEGACY_DEV_ROOT_HEX: &str =
    "0707070707070707070707070707070707070707070707070707070707070707";

/// Identity marker inside a vault directory: one line, 32 hex chars. Not
/// secret — it is the *name* the key is filed under, not the key.
const VAULT_ID_FILE: &str = "vault-id";

/// The vault database, probed to tell "no vault here yet" from "a vault made
/// before vaults had ids".
const VAULT_DB_FILE: &str = "vault.db";

/// Registry file inside the keystore: lines of `<id> <path>`.
const REGISTRY_FILE: &str = "vaults";

/// Bytes in a vault root. Fixed by the crypto suite; the seam rejects anything
/// else.
const ROOT_LEN: usize = 32;

/// Bytes in a vault id.
const ID_LEN: usize = 16;

/// One vault this machine knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultEntry {
    /// The vault's id: the keystore account, and the `vault-id` file's content.
    pub id: String,
    /// Where its directory was when it was registered.
    pub path: PathBuf,
}

/// Why a vault could not be keyed.
///
/// Typed rather than a string because two of these are conditions a user has
/// to act on differently, and because the pre-baseline refusal in
/// `sunrise-storage` (ADR-0018) set the precedent: a vault this build will not
/// open must say so in its own words, not surface as a corrupt read.
#[derive(Debug)]
pub enum VaultError {
    /// The vault predates per-vault roots: it has a database but no
    /// `vault-id`, so it was written under the one hardcoded root.
    ///
    /// Deliberately **not** opened by guessing that root. Doing so would keep
    /// every such vault keyed by a constant compiled into a binary anyone can
    /// download, which is the condition this module exists to end, and would
    /// hide from the user that their two "accounts" were never separate. The
    /// break is pre-1.0 and follows ADR-0018's precedent — `STORAGE_V = 13`
    /// already refuses older vaults outright — but unlike that one it is
    /// recoverable without data loss: the old root is a known constant, so the
    /// message hands it over and the vault opens with [`ENV_VAULT_ROOT`].
    PreMultiAccount {
        /// The vault directory.
        dir: PathBuf,
    },
    /// The vault names an id whose key is not in this keystore.
    ///
    /// The ordinary cause is a vault directory that travelled without its
    /// keystore — a backup, a copied home directory, a `scp`. The data is not
    /// damaged; it is ciphertext this machine cannot key.
    RootMissing {
        /// The id read from the vault directory.
        id: String,
        /// Where its key was looked for.
        keystore: PathBuf,
    },
    /// [`ENV_VAULT_ROOT`] was set to something that is not 64 hex characters.
    BadRootHex {
        /// How many characters were supplied.
        len: usize,
    },
    /// A key, marker or registry file could not be read or written.
    Io {
        /// What was being touched.
        path: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// A key or marker file exists but does not hold what it should.
    Malformed {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        detail: String,
    },
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreMultiAccount { dir } => write!(
                f,
                "the vault at {} was created before vaults had their own keys, \
                 when every vault shared one root compiled into this binary. \
                 It is not opened by guessing that root, because that would \
                 leave it readable by anyone holding a copy of `sunrise`. \
                 To open it once and move the work somewhere new:\n\
                 \x20   {ENV_VAULT_ROOT}={LEGACY_DEV_ROOT_HEX} sunrise export activity json > out.json\n\
                 Otherwise pick a fresh directory; this build makes a new key for it.",
                dir.display()
            ),
            Self::RootMissing { id, keystore } => write!(
                f,
                "no key for vault {id} in {}. The key never lives beside the \
                 data — a vault directory copied on its own is ciphertext. \
                 Bring the keystore over from the machine that made it, or \
                 supply the root directly with {ENV_VAULT_ROOT}=<64 hex chars>.",
                keystore.display()
            ),
            Self::BadRootHex { len } => write!(
                f,
                "{ENV_VAULT_ROOT} must be exactly {} hex characters ({ROOT_LEN} bytes); got {len}",
                ROOT_LEN * 2
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Malformed { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for VaultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Where per-vault keys live, from the process environment.
#[must_use]
pub fn keystore_dir() -> PathBuf {
    keystore_dir_in(
        std::env::var(ENV_KEYSTORE).ok().as_deref(),
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure form of [`keystore_dir`], so the precedence is testable without
/// touching the process environment — the same shape
/// [`sunrise_log::log_path_in`] uses for the log destination.
#[must_use]
pub fn keystore_dir_in(
    env_keystore: Option<&str>,
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }
    if let Some(explicit) = clean(env_keystore) {
        return PathBuf::from(explicit);
    }
    let base = clean(xdg_data_home).map_or_else(
        || {
            PathBuf::from(clean(home).unwrap_or("."))
                .join(".local")
                .join("share")
        },
        PathBuf::from,
    );
    base.join("sunrise").join("keys")
}

/// The root named by [`ENV_VAULT_ROOT`], if it is set to anything non-blank.
///
/// # Errors
/// [`VaultError::BadRootHex`] when it is set but is not 64 hex characters.
pub fn root_from_env() -> Result<Option<[u8; ROOT_LEN]>, VaultError> {
    let Ok(raw) = std::env::var(ENV_VAULT_ROOT) else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    from_hex::<ROOT_LEN>(raw)
        .map(Some)
        .ok_or(VaultError::BadRootHex { len: raw.len() })
}

/// The root for the vault at `vault_dir`, in the order the binary resolves it.
///
/// # Errors
/// Every [`VaultError`].
pub fn resolve(vault_dir: &Path, rng: &dyn Rng) -> Result<[u8; ROOT_LEN], VaultError> {
    if let Some(root) = root_from_env()? {
        return Ok(root);
    }
    open_or_create(vault_dir, &keystore_dir(), rng)
}

/// Load the root for `vault_dir` out of `keystore`, minting one if this is a
/// directory no vault has been made in yet.
///
/// Idempotent: the second call for a vault returns what the first one
/// generated, which is what lets a test resolve the same root a separate
/// `sunrise` process minted.
///
/// # Errors
/// [`VaultError::PreMultiAccount`] for a vault written before vault ids
/// existed, [`VaultError::RootMissing`] when this keystore does not hold the
/// named key, and I/O failures on either file.
pub fn open_or_create(
    vault_dir: &Path,
    keystore: &Path,
    rng: &dyn Rng,
) -> Result<[u8; ROOT_LEN], VaultError> {
    if let Some(id) = read_vault_id(vault_dir)? {
        let path = key_path(keystore, &id);
        return match read_hex_line::<ROOT_LEN>(&path)? {
            Some(root) => Ok(root),
            None => Err(VaultError::RootMissing {
                id,
                keystore: keystore.to_path_buf(),
            }),
        };
    }
    // No marker. Either nothing has ever been opened here, or this is a vault
    // from before vaults had ids — and those two must not be confused, because
    // minting a fresh key over an existing database would turn every
    // subsequent read into a SQLCipher failure with no explanation attached.
    if vault_dir.join(VAULT_DB_FILE).exists() {
        return Err(VaultError::PreMultiAccount {
            dir: vault_dir.to_path_buf(),
        });
    }

    let mut id_bytes = [0u8; ID_LEN];
    rng.fill_bytes(&mut id_bytes);
    let id = to_hex(&id_bytes);
    let mut root = [0u8; ROOT_LEN];
    rng.fill_bytes(&mut root);

    // Key first, marker second. A crash between them leaves an orphaned key
    // and a directory that still reads as fresh, which the next run recovers
    // from by minting again; the other order would leave a vault naming a key
    // that was never written, which is indistinguishable from a lost key.
    write_private(&key_path(keystore, &id), &to_hex(&root))?;
    write_line(&vault_dir.join(VAULT_ID_FILE), &id)?;
    register(keystore, &id, vault_dir)?;
    Ok(root)
}

/// The vaults this keystore has keys for, in registration order.
///
/// Best-effort by design: this backs a listing, and an unreadable registry
/// should print nothing rather than stop a `sunrise vaults` that was asked
/// precisely because something is confusing.
#[must_use]
pub fn registered(keystore: &Path) -> Vec<VaultEntry> {
    let Ok(body) = std::fs::read_to_string(keystore.join(REGISTRY_FILE)) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|line| {
            let (id, path) = line.split_once(' ')?;
            let path = path.trim();
            (!id.is_empty() && !path.is_empty()).then(|| VaultEntry {
                id: id.to_string(),
                path: PathBuf::from(path),
            })
        })
        .collect()
}

/// The id recorded in a vault directory, if it has one.
fn read_vault_id(vault_dir: &Path) -> Result<Option<String>, VaultError> {
    let path = vault_dir.join(VAULT_ID_FILE);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(VaultError::Io { path, source }),
    };
    let id = body.trim().to_string();
    if id.len() != ID_LEN * 2 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        // A marker that is not an id cannot be repaired by guessing: the key it
        // named is unfindable either way, so say what is wrong rather than
        // silently minting a second vault over the first one's data.
        return Err(VaultError::Malformed {
            path,
            detail: format!("expected {} hex characters naming a vault", ID_LEN * 2),
        });
    }
    Ok(Some(id))
}

fn key_path(keystore: &Path, id: &str) -> PathBuf {
    keystore.join(format!("{id}.key"))
}

/// Add `id` to the keystore's registry unless it is already listed.
///
/// A path that cannot be written on one line is simply not registered: the
/// registry backs a listing, and losing a row from `sunrise vaults` is a far
/// smaller failure than refusing to open the vault over it.
fn register(keystore: &Path, id: &str, vault_dir: &Path) -> Result<(), VaultError> {
    let Some(path) = vault_dir.to_str() else {
        return Ok(());
    };
    if path.contains(['\n', '\r']) {
        return Ok(());
    }
    if registered(keystore).iter().any(|e| e.id == id) {
        return Ok(());
    }
    let file = keystore.join(REGISTRY_FILE);
    let mut body = std::fs::read_to_string(&file).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(id);
    body.push(' ');
    body.push_str(path);
    body.push('\n');
    write_line(&file, body.trim_end_matches('\n'))
}

/// Read one line of `N * 2` hex characters, or `None` when the file is absent.
fn read_hex_line<const N: usize>(path: &Path) -> Result<Option<[u8; N]>, VaultError> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(VaultError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    from_hex::<N>(body.trim()).map(Some).ok_or_else(|| {
        // A truncated or edited key file must not read as "no key": that would
        // send the caller down the mint path and write a fresh key over a
        // vault whose data is keyed by the old one.
        VaultError::Malformed {
            path: path.to_path_buf(),
            detail: format!("expected {} hex characters", N * 2),
        }
    })
}

/// Create/replace `path` with owner-only permissions, the mode set **at
/// creation** so the secret is never briefly world-readable — the same rule,
/// and the same reason, as `sunrise_auth::FileStore`.
#[cfg(unix)]
fn write_private(path: &Path, line: &str) -> Result<(), VaultError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let io = |source| VaultError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io)?;
    f.write_all(line.as_bytes()).map_err(io)?;
    f.write_all(b"\n").map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(not(unix))]
fn write_private(path: &Path, line: &str) -> Result<(), VaultError> {
    // No mode bits to set. A Windows client should be using the OS credential
    // store, exactly as `sunrise_auth::FileStore` says.
    write_line(path, line)
}

fn write_line(path: &Path, line: &str) -> Result<(), VaultError> {
    let io = |source| VaultError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::write(path, format!("{line}\n")).map_err(io)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::SystemRng;

    /// Deterministic bytes, so a test can assert *which* root was written
    /// without reading a CSPRNG. The workspace forbids reaching for
    /// `rand::random` in production code; this is the injection point that
    /// makes that possible here too.
    #[derive(Debug)]
    struct Counting(std::sync::atomic::AtomicU8);

    impl Rng for Counting {
        fn fill_bytes(&self, dest: &mut [u8]) {
            use std::sync::atomic::Ordering;
            for b in dest.iter_mut() {
                *b = self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn counting() -> Counting {
        Counting(std::sync::atomic::AtomicU8::new(1))
    }

    fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn a_fresh_vault_gets_a_key_and_the_second_open_reuses_it() {
        let (vault, keys) = dirs();
        let first = open_or_create(vault.path(), keys.path(), &counting()).unwrap();
        let second = open_or_create(vault.path(), keys.path(), &counting()).unwrap();
        assert_eq!(
            first, second,
            "reopening a vault must not mint a second key"
        );
    }

    /// The whole point. Two vault directories must not be openable by one key.
    #[test]
    fn two_vaults_get_different_roots() {
        let keys = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let rng = SystemRng;
        let ra = open_or_create(a.path(), keys.path(), &rng).unwrap();
        let rb = open_or_create(b.path(), keys.path(), &rng).unwrap();
        assert_ne!(ra, rb, "two vaults sharing a root are one account");
        assert_ne!(
            ra, [7u8; ROOT_LEN],
            "and neither may be the old hardcoded constant"
        );
    }

    /// The key must not be reachable from the data. A vault directory copied
    /// on its own has to be ciphertext, or SQLCipher is decoration.
    #[test]
    fn the_key_is_not_written_into_the_vault_directory() {
        let (vault, keys) = dirs();
        let root = open_or_create(vault.path(), keys.path(), &SystemRng).unwrap();
        let hex = to_hex(&root);
        for entry in std::fs::read_dir(vault.path()).unwrap() {
            let path = entry.unwrap().path();
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                !body.contains(&hex),
                "the root leaked into {}",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (vault, keys) = dirs();
        open_or_create(vault.path(), keys.path(), &SystemRng).unwrap();
        let id = read_vault_id(vault.path()).unwrap().unwrap();
        let mode = std::fs::metadata(key_path(keys.path(), &id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// A vault directory that travelled without its keystore is a typed,
    /// explanatory failure — not a SQLCipher error from three layers down, and
    /// not a fresh key written over data it cannot open.
    #[test]
    fn a_vault_whose_key_is_absent_says_so() {
        let (vault, keys) = dirs();
        let id = {
            open_or_create(vault.path(), keys.path(), &SystemRng).unwrap();
            read_vault_id(vault.path()).unwrap().unwrap()
        };
        std::fs::remove_file(key_path(keys.path(), &id)).unwrap();

        let err = open_or_create(vault.path(), keys.path(), &SystemRng).unwrap_err();
        assert!(matches!(err, VaultError::RootMissing { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(&id), "the message must name the vault: {msg}");
        assert!(msg.contains(ENV_VAULT_ROOT), "and the remedy: {msg}");
    }

    /// ADR-0018's precedent: a vault this build will not open refuses in its
    /// own words. Unlike that one the remedy is not "start over" — the old
    /// root was a constant, so the message hands it back.
    #[test]
    fn a_vault_from_before_vault_ids_is_refused_with_the_way_out() {
        let (vault, keys) = dirs();
        std::fs::write(vault.path().join(VAULT_DB_FILE), b"not really a db").unwrap();

        let err = open_or_create(vault.path(), keys.path(), &SystemRng).unwrap_err();
        assert!(matches!(err, VaultError::PreMultiAccount { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(LEGACY_DEV_ROOT_HEX), "{msg}");
        assert!(msg.contains(ENV_VAULT_ROOT), "{msg}");
        assert!(
            !keys.path().join(REGISTRY_FILE).exists(),
            "a refused vault must not be registered"
        );
    }

    /// An empty directory is not an old vault. The two are told apart by the
    /// database, not by the directory being non-empty — a stray
    /// `credentials.json` from a `sunrise login` must not brick the vault.
    #[test]
    fn a_directory_with_other_files_but_no_database_is_still_fresh() {
        let (vault, keys) = dirs();
        std::fs::write(vault.path().join("credentials.json"), b"{}").unwrap();
        assert!(open_or_create(vault.path(), keys.path(), &SystemRng).is_ok());
    }

    /// A truncated key file must not read as "no key" — that would send the
    /// caller down the mint path and write a new key over an old vault.
    #[test]
    fn a_corrupt_key_file_is_an_error_not_an_absence() {
        let (vault, keys) = dirs();
        open_or_create(vault.path(), keys.path(), &SystemRng).unwrap();
        let id = read_vault_id(vault.path()).unwrap().unwrap();
        std::fs::write(key_path(keys.path(), &id), b"deadbeef\n").unwrap();

        let err = open_or_create(vault.path(), keys.path(), &SystemRng).unwrap_err();
        assert!(matches!(err, VaultError::Malformed { .. }), "{err:?}");
    }

    #[test]
    fn a_corrupt_marker_is_an_error_not_a_second_vault() {
        let (vault, keys) = dirs();
        std::fs::write(vault.path().join(VAULT_ID_FILE), b"hello\n").unwrap();
        let err = open_or_create(vault.path(), keys.path(), &SystemRng).unwrap_err();
        assert!(matches!(err, VaultError::Malformed { .. }), "{err:?}");
    }

    #[test]
    fn every_vault_is_listed_once() {
        let keys = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        open_or_create(a.path(), keys.path(), &SystemRng).unwrap();
        open_or_create(b.path(), keys.path(), &SystemRng).unwrap();
        // A reopen must not add a second row.
        open_or_create(a.path(), keys.path(), &SystemRng).unwrap();

        let rows = registered(keys.path());
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().any(|e| e.path == a.path()));
        assert!(rows.iter().any(|e| e.path == b.path()));
        assert_ne!(rows[0].id, rows[1].id);
    }

    #[test]
    fn an_absent_registry_lists_nothing_rather_than_failing() {
        let keys = tempfile::tempdir().unwrap();
        assert!(registered(keys.path()).is_empty());
    }

    #[test]
    fn the_keystore_location_follows_xdg_then_home() {
        assert_eq!(
            keystore_dir_in(Some("/explicit"), Some("/data"), Some("/home/u")),
            PathBuf::from("/explicit")
        );
        assert_eq!(
            keystore_dir_in(None, Some("/data"), Some("/home/u")),
            PathBuf::from("/data/sunrise/keys")
        );
        assert_eq!(
            keystore_dir_in(None, None, Some("/home/u")),
            PathBuf::from("/home/u/.local/share/sunrise/keys")
        );
        // A blank variable behaves as unset, matching `log_path_in`.
        assert_eq!(
            keystore_dir_in(Some("  "), Some(""), Some("/home/u")),
            PathBuf::from("/home/u/.local/share/sunrise/keys")
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_the_wrong_length() {
        let bytes = [0xab_u8; ROOT_LEN];
        assert_eq!(from_hex::<ROOT_LEN>(&to_hex(&bytes)), Some(bytes));
        assert_eq!(from_hex::<ROOT_LEN>("ab"), None);
        assert_eq!(from_hex::<ROOT_LEN>(&"zz".repeat(ROOT_LEN)), None);
        assert_eq!(
            from_hex::<ROOT_LEN>(LEGACY_DEV_ROOT_HEX),
            Some([7u8; ROOT_LEN]),
            "the constant quoted in the error must be the root it names"
        );
    }
}
