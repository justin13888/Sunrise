//! Every `sunrise-server admin` command, run as the binary an operator runs,
//! against a temporary data dir.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sunrise_server::store::NewDevice;
use sunrise_server::{Store, Subject};

const NOW: u64 = 1_800_000_000_000;

struct Install {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Install {
    /// A data dir holding one account with two devices, and a config naming it.
    fn new() -> (Self, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let config = dir.path().join("sunrise.toml");
        std::fs::write(
            &config,
            format!("[storage]\ndata_dir = {:?}\n", data.display().to_string()),
        )
        .unwrap();
        let store = Store::open(Some(&data.join("sunrise.db"))).unwrap();
        let account = store
            .resolve_account(&Subject::new("https://idp.example", "alice"), true, NOW)
            .unwrap()
            .account_id;
        let device = |name: &str| {
            store
                .register_device(
                    &account,
                    &NewDevice {
                        device_pub_s: "k".into(),
                        vault_device_id: None,
                        device_pub_d: None,
                        device_cert: None,
                        nickname: name.into(),
                        platform: "linux".into(),
                        app_version: None,
                    },
                    NOW,
                )
                .unwrap()
                .device_id
        };
        let phone = device("phone");
        device("laptop");
        (Self { dir, config }, account, phone)
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sunrise-server"))
            .arg("admin")
            .arg("-c")
            .arg(&self.config)
            .args(args)
            .env_remove("SUNRISE_CONFIG")
            .output()
            .unwrap()
    }

    /// Run with `--json`, require success, and parse the answer.
    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn doctor_checks_the_install_and_passes_on_a_sound_one() {
    let (install, _, _) = Install::new();
    let report = install.json(&["doctor"]);
    assert_eq!(report["ok"], true, "{report}");
    let names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    for want in [
        "config",
        "database",
        "encryption",
        "storage",
        "blob_root",
        "push",
        "protocol",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert!(
        !install.data().join(".sunrise-doctor.tmp").exists(),
        "the probe file is removed"
    );

    // Without --json the same report is lines a person reads.
    let text = install.run(&["doctor"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("check: storage"));
}

#[test]
fn stats_counts_what_the_data_dir_holds() {
    let (install, _, _) = Install::new();
    let stats = install.json(&["stats"]);
    assert_eq!(stats["accounts"], 1);
    assert_eq!(stats["devices_active"], 2);
    assert_eq!(stats["relay_frames"], 0);
    assert_eq!(stats["blob_files"], 0);
}

#[test]
fn account_list_and_show_report_ids_and_never_emails() {
    let (install, account, _) = Install::new();
    let list = install.json(&["account", "list"]);
    assert_eq!(list["accounts"][0]["account_id"], account.as_str());
    let shown = install.json(&["account", "show", &account]);
    assert_eq!(shown["devices_active"], 2);
    assert!(!shown.to_string().contains("alice"), "{shown}");

    let missing = install.run(&["account", "show", "NOPE"]);
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn account_delete_marks_and_gc_erases_only_after_the_grace_period() {
    let (install, account, _) = Install::new();
    let marked = install.json(&["account", "delete", &account]);
    assert!(
        marked["erase_after_ms"].as_u64().unwrap() > marked["requested_at_ms"].as_u64().unwrap()
    );
    assert!(install.json(&["account", "show", &account])["delete_requested_at_ms"].is_u64());

    // Marked now, so the grace period has thirty days to run.
    let dry = install.json(&["gc", "--dry-run"]);
    assert_eq!(
        (dry["dry_run"].clone(), dry["accounts_erased"].clone()),
        (Value::Bool(true), 0.into())
    );
    let now = install.json(&["gc", "--now"]);
    assert_eq!(now["accounts_erased"], 0);
    assert_eq!(install.json(&["stats"])["accounts"], 1);

    let erased = install.json(&["account", "delete", &account, "--immediately"]);
    assert_eq!(erased["erased"], true);
    assert_eq!(install.json(&["stats"])["accounts"], 0);
    assert_eq!(install.json(&["stats"])["devices_active"], 0);
}

#[test]
fn gc_requires_choosing_dry_run_or_now() {
    let (install, _, _) = Install::new();
    let out = install.run(&["gc"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn device_revoke_revokes_once() {
    let (install, account, phone) = Install::new();
    let revoked = install.json(&["device", "revoke", &phone]);
    assert_eq!(revoked["account_id"], account.as_str());
    assert_eq!(install.json(&["stats"])["devices_revoked"], 1);
    assert_eq!(
        install.run(&["device", "revoke", &phone]).status.code(),
        Some(1)
    );
    assert_eq!(
        install.run(&["device", "revoke", "NOPE"]).status.code(),
        Some(1)
    );
}

#[test]
fn backup_writes_a_database_that_opens_and_holds_the_same_rows() {
    let (install, account, _) = Install::new();
    let blob = install.data().join("blobs/committed/aa/manifests");
    std::fs::create_dir_all(&blob).unwrap();
    std::fs::write(blob.join("bb"), "1 3").unwrap();
    std::fs::write(blob.join("partial.bin.tmp"), "x").unwrap();

    let dest = install.dir.path().join("backup");
    let out = install.json(&["backup", dest.to_str().unwrap()]);
    assert_eq!(out["blob_files"], 1, "{out}");
    let copy = Store::open(Some(&dest.join("sunrise.db"))).unwrap();
    assert!(copy.account_summary(&account).unwrap().is_some());
    assert!(dest.join("blobs/committed/aa/manifests/bb").exists());
    assert!(!Path::new(&dest.join("blobs/committed/aa/manifests/partial.bin.tmp")).exists());

    assert_eq!(
        install
            .run(&["backup", dest.to_str().unwrap()])
            .status
            .code(),
        Some(1),
        "an existing destination is refused, not overwritten"
    );
}

#[test]
fn a_config_with_no_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("sunrise.toml");
    std::fs::write(&config, "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sunrise-server"))
        .args(["admin", "-c"])
        .arg(&config)
        .arg("stats")
        .env_remove("SUNRISE_CONFIG")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("data_dir"));
}
