//! The relay database at rest, through the binary an operator runs: the
//! startup refusals a key can cause, and `admin backup`, a restore from it,
//! and `admin rekey` on an encrypted install.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use sunrise_server::store::{pre_encryption_copy, DbKey, DEFAULT_BUSY_TIMEOUT};
use sunrise_server::{Store, Subject};

const NOW: u64 = 1_800_000_000_000;
const EX_CONFIG: i32 = 78;

/// A data dir, a key file beside it (outside it), and a config naming both.
struct Install {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Install {
    /// `encrypt` writes `[storage] encrypt = true` and a `key_file` holding
    /// `key_hex` with `mode`.
    fn new(encrypt: bool, key_hex: &str, mode: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        let config = dir.path().join("sunrise.toml");
        let install = Self { dir, config };
        Install::write_key(&install.key_file(), key_hex, mode);
        let encryption = if encrypt {
            format!(
                "encrypt = true\nkey_file = {:?}\n",
                install.key_file().display().to_string()
            )
        } else {
            String::new()
        };
        let toml = format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_dir = {:?}\n{encryption}",
            install.data().display().to_string()
        );
        std::fs::write(&install.config, toml).unwrap();
        install
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn db(&self) -> PathBuf {
        self.data().join("sunrise.db")
    }

    fn key_file(&self) -> PathBuf {
        self.dir.path().join("db.key")
    }

    fn write_key(path: &Path, hex: &str, mode: u32) {
        let _ = std::fs::remove_file(path);
        std::fs::write(path, format!("{hex}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    /// Open the store as the server would, and give it an account.
    fn seed(&self, key: Option<&DbKey>) -> String {
        let store = Store::open_keyed(Some(&self.db()), DEFAULT_BUSY_TIMEOUT, key).unwrap();
        store
            .resolve_account(&Subject::new("https://idp.example", "alice"), true, NOW)
            .unwrap()
            .account_id
    }

    /// Start the server, and return how it exited. Every case here is a
    /// refusal, which comes before the listener binds; one that starts
    /// serving instead is killed after a bound and fails the test.
    fn start(&self) -> (Option<i32>, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sunrise-server"))
            .arg("-c")
            .arg(&self.config)
            .env_remove("SUNRISE_CONFIG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        for _ in 0..300 {
            if child.try_wait().unwrap().is_some() {
                let out = child.wait_with_output().unwrap();
                return (
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        panic!("the server started instead of refusing");
    }

    fn admin(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sunrise-server"))
            .args(["admin", "--json", "-c"])
            .arg(&self.config)
            .args(args)
            .env_remove("SUNRISE_CONFIG")
            .output()
            .unwrap()
    }

    fn admin_json(&self, args: &[&str]) -> Value {
        let out = self.admin(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn hex(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn key(byte: u8) -> DbKey {
    DbKey::from_bytes([byte; 32])
}

/// Each way a key can be wrong stops the server with `EX_CONFIG`, naming
/// what is wrong, before it writes to the database.
#[test]
fn a_missing_wrong_or_exposed_key_refuses_startup_with_exit_78() {
    // The key file the config names does not exist.
    let install = Install::new(true, &hex(1), 0o600);
    std::fs::remove_file(install.key_file()).unwrap();
    let (code, log) = install.start();
    assert_eq!(code, Some(EX_CONFIG), "{log}");
    assert!(log.contains("key_file"), "{log}");
    assert!(!install.db().exists(), "nothing was created");

    // An encrypted database, and a config with encryption off.
    let install = Install::new(false, &hex(1), 0o600);
    install.seed(Some(&key(1)));
    let (code, log) = install.start();
    assert_eq!(code, Some(EX_CONFIG), "{log}");
    assert!(log.contains("encrypt = true"), "{log}");

    // The wrong key.
    let install = Install::new(true, &hex(2), 0o600);
    install.seed(Some(&key(1)));
    let before = std::fs::read(install.db()).unwrap();
    let (code, log) = install.start();
    assert_eq!(code, Some(EX_CONFIG), "{log}");
    assert!(log.contains("does not open"), "{log}");
    assert_eq!(std::fs::read(install.db()).unwrap(), before);

    // A key file others can read.
    #[cfg(unix)]
    {
        let install = Install::new(true, &hex(1), 0o644);
        let (code, log) = install.start();
        assert_eq!(code, Some(EX_CONFIG), "{log}");
        assert!(log.contains("chmod 600"), "{log}");
        assert!(!install.db().exists());
    }
}

/// An install switched to `encrypt = true` is migrated by the first command
/// that opens it, and `doctor` then points at the plaintext copy left behind.
#[test]
fn turning_encryption_on_migrates_the_install_and_doctor_flags_the_plaintext_copy() {
    let install = Install::new(true, &hex(5), 0o600);
    let account = install.seed(None);

    let report = install.admin(&["doctor"]);
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    let encryption = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "encryption")
        .unwrap()
        .clone();
    assert_eq!(encryption["status"], "fail", "{report}");
    assert_eq!(report["ok"], false);

    let kept = pre_encryption_copy(&install.db());
    assert!(kept.exists());
    std::fs::remove_file(&kept).unwrap();
    let report = install.admin_json(&["doctor"]);
    assert_eq!(report["ok"], true, "{report}");

    let store =
        Store::open_keyed(Some(&install.db()), DEFAULT_BUSY_TIMEOUT, Some(&key(5))).unwrap();
    assert!(store.account_summary(&account).unwrap().is_some());
}

/// `admin backup` of an encrypted install, then the restore the operator
/// documentation gives: stop, replace the data dir with the backup, start.
#[test]
fn an_encrypted_backup_restores_into_a_working_install() {
    let install = Install::new(true, &hex(6), 0o600);
    let account = install.seed(Some(&key(6)));
    let manifests = install.data().join("blobs/committed/aa/manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(manifests.join("bb"), "1 3").unwrap();

    let dest = install.dir.path().join("backup");
    let out = install.admin_json(&["backup", dest.to_str().unwrap()]);
    assert_eq!(out["encrypted"], true, "{out}");
    assert!(matches!(
        Store::open(Some(&dest.join("sunrise.db"))),
        Err(sunrise_server::StoreError::KeyRequired { .. })
    ));

    std::fs::remove_dir_all(install.data()).unwrap();
    std::fs::create_dir_all(install.data()).unwrap();
    std::fs::copy(dest.join("sunrise.db"), install.db()).unwrap();
    copy_dir(&dest.join("blobs"), &install.data().join("blobs"));

    let shown = install.admin_json(&["account", "show", &account]);
    assert_eq!(shown["account_id"], account.as_str());
    assert!(install
        .data()
        .join("blobs/committed/aa/manifests/bb")
        .exists());
    let report = install.admin_json(&["doctor"]);
    assert_eq!(report["ok"], true, "{report}");
}

/// `admin rekey`, then the config pointed at the new key: the old key stops
/// opening the database and the new one opens it with every row.
#[test]
fn rekey_rotates_to_a_new_key_file() {
    let install = Install::new(true, &hex(7), 0o600);
    let account = install.seed(Some(&key(7)));
    let next = install.dir.path().join("db.key.next");
    Install::write_key(&next, &hex(8), 0o600);

    let out = install.admin_json(&["rekey", next.to_str().unwrap()]);
    assert_eq!(out["rekeyed"], true, "{out}");

    // The config still names the old key, which no longer opens it.
    let refused = install.admin(&["stats"]);
    assert_eq!(refused.status.code(), Some(EX_CONFIG));
    std::fs::rename(&next, install.key_file()).unwrap();
    let shown = install.admin_json(&["account", "show", &account]);
    assert_eq!(shown["account_id"], account.as_str());

    // A new key file others can read is refused before anything is rewritten.
    #[cfg(unix)]
    {
        let loose = install.dir.path().join("loose.key");
        Install::write_key(&loose, &hex(9), 0o644);
        assert_eq!(
            install
                .admin(&["rekey", loose.to_str().unwrap()])
                .status
                .code(),
            Some(1)
        );
        install.admin_json(&["stats"]);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
