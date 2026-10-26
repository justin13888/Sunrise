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

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use serde_json::Value;
// The tokio clock, as `latency.rs` uses: the workspace bans `std`'s
// `Instant::now`.
use tokio::time::Instant;

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
        self.command(args).output().expect("run sunrise")
    }

    /// Start `sunrise <args>` as this device and leave it running, with
    /// stdin, stdout and stderr piped: for a command that prints something
    /// the other device needs while it waits (`sunrise pair join`'s code).
    ///
    /// # Panics
    /// When the process cannot be started.
    #[must_use]
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start sunrise")
    }

    /// `sunrise <args>` as this device, not yet started.
    fn command(&self, args: &[&str]) -> Command {
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
        cmd
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
    /// When the command fails, with its stdout, its stderr and the tail of
    /// this device's log.
    pub async fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args).await;
        assert!(out.status.success(), "{}", self.failure(args, &out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Why `sunrise <args>` failed: its stdout and stderr, then the last
    /// [`LOG_TAIL`] lines of `log.ndjson`. The log is the only record of the
    /// session errors and backoffs behind a failed sync, and it goes with
    /// the tempdir as the panic unwinds, so this message is its one chance
    /// to be read.
    fn failure(&self, args: &[&str], out: &Output) -> String {
        let log_path = self.vault.with_extension("log.ndjson");
        let log = std::fs::read_to_string(&log_path).map_or_else(
            |e| format!("(no log at {}: {e})", log_path.display()),
            |text| {
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n")
            },
        );
        format!(
            "`sunrise {}` failed on {}:\n{}\n{}\n--- last {LOG_TAIL} lines of {} ---\n{log}",
            args.join(" "),
            self.vault.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            log_path.display(),
        )
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

    /// `sync --once`, required to end live within [`WAIT`].
    ///
    /// # Panics
    /// As [`Device::sync_by`] does, with a deadline [`WAIT`] from now.
    pub async fn sync(&self) {
        self.sync_by(Instant::now() + WAIT).await;
    }

    /// `sync --once`, re-run while it fails only because its own 30 s
    /// budget ran out, until `deadline`. One reconnect cascade under load
    /// outlasts a single budget; any other failure (a usage error, a
    /// refused or degraded session) fails at once.
    ///
    /// # Panics
    /// When `sync --once` fails other than by timing out, or still times out
    /// at `deadline`, with its last output and the device's log.
    async fn sync_by(&self, deadline: Instant) {
        const ARGS: [&str; 2] = ["sync", "--once"];
        // The CLI's message for a wait that ran out its budget, which is
        // `DEADLINE_MS` (30 s) in `sunrise-cli`'s `cmd/plumbing.rs`.
        let timed_out = sunrise_cli::i18n::strings::sync::timed_out(30);
        loop {
            let out = self.run(&ARGS).await;
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(stdout.contains("sync: live"), "{stdout}");
                return;
            }
            let retry = String::from_utf8_lossy(&out.stderr).contains(&timed_out)
                && Instant::now() < deadline;
            assert!(retry, "{}", self.failure(&ARGS, &out));
        }
    }

    /// `sunrise --json sync status`: the badge code and the outbox depth.
    async fn sync_status(&self) -> (String, u64) {
        let s = self.json(&["sync", "status"]).await;
        let badge = s["badge"].as_str().expect("a badge code").to_owned();
        let pending = s["pending"].as_u64().expect("a pending count");
        (badge, pending)
    }
}

/// How many lines of a device's `log.ndjson` a failed command's message ends
/// with.
const LOG_TAIL: usize = 100;

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
/// Recovery is one way the CLI adds a device without moving files between
/// machines; `sunrise pair join` / `sunrise pair add` over the relay is the
/// other (`tests/cli_pair_relay.rs`), and `sunrise pair`'s four files the way
/// with no relay at all.
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
///
/// Rounds of `sync --once` on A then B, until both report nothing pending,
/// then one more round in which neither device's pending count or badge
/// changes: B's last sends reach A in it, and a change either side still
/// had in hand shows. All of it within [`WAIT`].
///
/// # Panics
/// When the two have not converged within [`WAIT`], with both devices' last
/// status.
pub async fn settle(a: &Device, b: &Device) {
    let deadline = Instant::now() + WAIT;
    let round = || async {
        a.sync_by(deadline).await;
        b.sync_by(deadline).await;
        (a.sync_status().await, b.sync_status().await)
    };
    let mut last = round().await;
    loop {
        let drained = last.0 .1 == 0 && last.1 .1 == 0;
        let next = round().await;
        if drained && next == last {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "A and B did not converge within {WAIT:?}: A {:?}, B {:?}",
            next.0,
            next.1
        );
        last = next;
    }
}

/// Generous: every step is a process start against a relay.
pub const WAIT: Duration = Duration::from_secs(180);

/// A `sunrise` process still running, and the `--json` events it prints.
/// The shape `cli_pair_relay.rs` drives pairing with.
#[derive(Debug)]
pub struct Running {
    child: Child,
    events: Option<mpsc::Receiver<Value>>,
}

impl Running {
    /// Start `sunrise <args>` as `device`, reading its `--json` events.
    ///
    /// # Panics
    /// When the process cannot be started.
    #[must_use]
    pub fn start(device: &Device, args: &[&str]) -> Self {
        let mut child = device.spawn(args);
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            events: Some(rx),
        }
    }

    /// The next event, which must be `name`.
    ///
    /// # Panics
    /// When no event arrives in time, or it is not `name`.
    pub async fn event(&mut self, name: &str) -> Value {
        let rx = self.events.take().expect("one wait at a time");
        let (rx, got) = tokio::task::spawn_blocking(move || {
            let got = rx.recv_timeout(WAIT);
            (rx, got)
        })
        .await
        .expect("join the reader");
        self.events = Some(rx);
        let event = match got {
            Ok(event) => event,
            // The process closed its stdout: say how it ended, which is the
            // only account of why the event never came.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.child.kill();
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
                }
                let status = self.child.wait().ok();
                panic!("no {name:?} event: the process ended ({status:?}); stderr:\n{stderr}")
            }
            Err(e) => panic!("no {name:?} event ({e})"),
        };
        assert_eq!(event["event"], name, "{event}");
        event
    }

    /// Wait for it to exit, successfully.
    ///
    /// # Panics
    /// When it does not exit in time, or fails.
    pub async fn finish(mut self) -> Output {
        drop(self.child.stdin.take());
        let out = tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || self.child.wait_with_output()),
        )
        .await
        .expect("the command exits")
        .expect("join the process")
        .expect("wait for sunrise");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

/// Pair a vault-less `b` onto `a`'s account over the relay: `sunrise pair
/// join` on `b`, `sunrise pair add` on `a`, both confirming the SAS.
///
/// # Panics
/// When either side fails.
pub async fn pair_over_relay(a: &Device, b: &Device) {
    let mut join = Running::start(
        b,
        &[
            "--json",
            "pair",
            "join",
            "--yes",
            "--email",
            "a@example.com",
        ],
    );
    let code = join.event("code").await["code"]
        .as_str()
        .expect("the code is text")
        .to_owned();
    let mut add = Running::start(a, &["--json", "pair", "add", &code, "--yes"]);
    add.event("sas").await;
    join.event("sas").await;
    add.event("paired").await;
    join.event("paired").await;
    add.finish().await;
    join.finish().await;
}
