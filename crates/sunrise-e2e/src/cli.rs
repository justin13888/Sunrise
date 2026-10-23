//! Driving the real `sunrise` binary against a real relay.
//!
//! The rest of this crate opens [`sunrise_core::Core`] in-process. These
//! helpers do what a person at two terminals does instead: each call is one
//! `sunrise` process with its own vault directory, keystore and environment,
//! talking to the relay [`crate::spawn_relay`] booted. So a scenario built on
//! them proves the CLI end to end — argument parsing, the vault lock, the
//! sealed undo history, the sync driver, the relay — and not merely the library
//! underneath it. It is how stabilisation is shown on a host with no Apple
//! tooling (`docs/10-cross-cutting/testing.md` §The CLI end to end).
//!
//! # Which binary
//!
//! `SUNRISE_CLI_BIN` when it is set. Otherwise the binary is built — `cargo
//! build -p sunrise-cli --bin sunrise`, a no-op when it is fresh — once per
//! test process, into the target directory this test binary was built in, so
//! a scenario never runs a stale `sunrise` against new code.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use serde_json::Value;

/// The bearer the harness relay's `NullVerifier` accepts, as any bearer.
pub const SELF_HOST_BEARER: &str = "self-host";

/// The `sunrise` binary under test.
///
/// # Panics
/// When the build fails, naming why.
#[must_use]
pub fn binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(p) = std::env::var_os("SUNRISE_CLI_BIN") {
            return PathBuf::from(p);
        }
        // target/<profile>/deps/<this test> -> target/<profile>/sunrise
        let exe = std::env::current_exe().expect("the test binary's path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("a target/<profile>/deps layout")
            .to_path_buf();
        let target_dir = profile_dir.parent().expect("a target directory");
        let release = profile_dir.file_name().is_some_and(|n| n == "release");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut build = Command::new(cargo);
        build
            .args(["build", "--quiet", "-p", "sunrise-cli", "--bin", "sunrise"])
            .env("CARGO_TARGET_DIR", target_dir);
        if release {
            build.arg("--release");
        }
        let status = build.status().expect("run cargo build");
        assert!(status.success(), "building the sunrise binary failed");
        profile_dir.join(format!("sunrise{}", std::env::consts::EXE_SUFFIX))
    })
}

/// One device: a vault directory and the environment its commands run in.
#[derive(Debug, Clone)]
pub struct Device {
    /// The vault directory.
    pub vault: PathBuf,
    /// The relay origin, when this device syncs.
    pub relay: Option<String>,
    /// `SUNRISE_TZ` for every command, when set.
    pub zone: Option<String>,
    /// `SUNRISE_SYNC_TOKEN`, when set.
    pub bearer: Option<String>,
}

impl Device {
    /// A device whose vault lives in `vault`, syncing to `relay`.
    #[must_use]
    pub fn new(vault: &Path, relay: &str) -> Self {
        Self {
            vault: vault.to_path_buf(),
            relay: Some(relay.to_owned()),
            zone: Some("UTC".to_owned()),
            bearer: None,
        }
    }

    /// The same device reading in `zone`.
    #[must_use]
    pub fn in_zone(&self, zone: &str) -> Self {
        Self {
            zone: Some(zone.to_owned()),
            ..self.clone()
        }
    }

    /// Run `sunrise <args>` as this device, with `stdin` piped in.
    ///
    /// Blocking, so call it from [`tokio::task::spawn_blocking`] (as [`Device::ok`]
    /// does) while the relay serves on the runtime.
    #[must_use]
    pub fn run_blocking(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(binary());
        cmd.args(args)
            .env("SUNRISE_VAULT", &self.vault)
            .env("SUNRISE_KEYSTORE", self.vault.with_extension("keys"))
            .env("SUNRISE_LOG_FILE", self.vault.with_extension("log.ndjson"))
            .env("XDG_CONFIG_HOME", self.vault.with_extension("config"))
            .env_remove("SUNRISE_VAULT_ROOT")
            .env_remove("SUNRISE_SYNC_DEVICE_ID")
            .env_remove("SUNRISE_OIDC_ISSUER")
            .env_remove("SUNRISE_OIDC_CLIENT_ID");
        match &self.relay {
            Some(url) => cmd.env("SUNRISE_SYNC_URL", url),
            None => cmd.env_remove("SUNRISE_SYNC_URL"),
        };
        match &self.zone {
            Some(z) => cmd.env("SUNRISE_TZ", z),
            None => cmd.env_remove("SUNRISE_TZ"),
        };
        match &self.bearer {
            Some(b) => cmd.env("SUNRISE_SYNC_TOKEN", b),
            None => cmd.env_remove("SUNRISE_SYNC_TOKEN"),
        };
        cmd.output().expect("run sunrise")
    }

    /// Run `sunrise <args>` off the runtime's worker threads.
    pub async fn run(&self, args: &[&str]) -> Output {
        let me = self.clone();
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            me.run_blocking(&refs)
        })
        .await
        .expect("join the sunrise process")
    }

    /// Run and require success; returns stdout.
    ///
    /// # Panics
    /// When the command fails, with its stdout and stderr.
    pub async fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args).await;
        assert!(
            out.status.success(),
            "`sunrise {}` failed on {}:\n{}\n{}",
            args.join(" "),
            self.vault.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run with `--json` and parse the document.
    ///
    /// # Panics
    /// When the command fails or prints something that is not JSON.
    pub async fn json(&self, args: &[&str]) -> Value {
        let mut with = vec!["--json"];
        with.extend_from_slice(args);
        let out = self.ok(&with).await;
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
    }

    /// One task, as `sunrise --json show` prints it.
    pub async fn task(&self, id: &str) -> Value {
        self.json(&["show", id]).await
    }

    /// `sync --once`, required to succeed.
    pub async fn sync(&self) {
        let out = self.ok(&["sync", "--once"]).await;
        assert!(out.contains("sync: live"), "{out}");
    }
}

/// The id a create command printed: the first word of its output.
///
/// # Panics
/// When there is no output.
#[must_use]
pub fn first_id(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .next()
        .expect("the command printed an id")
        .to_owned()
}

/// The 24 recovery words `sunrise bootstrap` printed, in order: the indented
/// rows of four between its heading and its footer.
///
/// # Panics
/// When there are not 24 of them.
#[must_use]
pub fn recovery_words(stdout: &str) -> Vec<String> {
    let words: Vec<String> = stdout
        .lines()
        .filter(|l| l.starts_with("    "))
        .map(str::split_whitespace)
        .filter_map(|w| {
            let row: Vec<&str> = w.collect();
            (row.len() == 4
                && row
                    .iter()
                    .all(|x| x.chars().all(|c| c.is_ascii_lowercase())))
            .then_some(row)
        })
        .flatten()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        words.len(),
        24,
        "bootstrap printed no recovery code: {stdout}"
    );
    words
}

/// Give the device at `vault` a stored sign-in, as `sunrise login` would
/// leave one, so `sunrise bootstrap` can run against the self-host relay.
///
/// # Panics
/// When the store cannot be written.
pub fn sign_in(vault: &Path) {
    std::fs::create_dir_all(vault).expect("the vault directory");
    let now_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0);
    let creds = sunrise_auth::Credentials::new(SELF_HOST_BEARER.into(), None, Some(3600), now_ms);
    sunrise_auth::store::CredentialStore::save(&sunrise_cli::login::store_for(vault), &creds)
        .expect("store the sign-in");
}

/// Device A, created with `sunrise bootstrap`, and device B, recovered from
/// the 24 words A printed: two devices on one account, both level with the
/// relay at `relay`, vaults under `root`.
///
/// Recovery is how the CLI adds a device without moving files between
/// machines today; `sunrise pair`'s four files are the other way, and the
/// relay-mediated pairing state machine replaces both later.
pub async fn bootstrap_and_recover(root: &Path, relay: &str) -> (Device, Device) {
    let a = Device::new(&root.join("a"), relay);
    sign_in(&a.vault);
    let boot = a.ok(&["bootstrap", "a@example.com"]).await;
    let words = recovery_words(&boot);
    a.sync().await;

    let mut b = Device::new(&root.join("b"), relay);
    // `recover` signs in again first unless it is handed a bearer; the
    // self-host relay is the one configuration exempt from the step-up.
    b.bearer = Some(SELF_HOST_BEARER.to_owned());
    let mut args = vec!["recover"];
    args.extend(words.iter().map(String::as_str));
    b.ok(&args).await;
    // One account: both devices name the same genesis identity.
    assert_eq!(
        a.json(&["identity"]).await["account"],
        b.json(&["identity"]).await["account"],
        "B joined A's account"
    );
    b.sync().await;
    (a, b)
}

/// Sync both ways until neither has anything left to send or receive.
pub async fn settle(a: &Device, b: &Device) {
    a.sync().await;
    b.sync().await;
    a.sync().await;
    b.sync().await;
}
