//! `login`, `whoami` and `logout` through the **real binary**: the dispatch in
//! `cmd/account.rs`, what each verb prints, and what it leaves in the vault
//! directory. The sign-in flow itself is `sunrise-auth`'s, tested there; a
//! `login` here stops at its configuration, before any network or browser.

use std::path::Path;
use std::process::{Command, Output};

use sunrise_auth::{CredentialStore, Credentials, FileStore, CREDENTIALS_FILE};
use sunrise_cli::login::{ENV_CLIENT_ID, ENV_ISSUER};

/// The binary with no OIDC configuration, whatever the test process exported.
fn sunrise(vault: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sunrise"));
    c.args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", vault.join("keystore"))
        .env("SUNRISE_LOG_FILE", vault.join("log.ndjson"))
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_SYNC_TOKEN")
        .env_remove("SUNRISE_VAULT_ROOT")
        .env_remove(ENV_ISSUER)
        .env_remove(ENV_CLIENT_ID);
    c
}

fn run(vault: &Path, args: &[&str]) -> Output {
    sunrise(vault, args).output().expect("run sunrise")
}

/// Run and require success, returning stdout without its trailing newline.
fn ok(vault: &Path, args: &[&str]) -> String {
    let out = run(vault, args);
    assert!(
        out.status.success(),
        "`sunrise {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// A stored login whose token expires `from_now_ms` from this moment.
fn store_login(vault: &Path, from_now_ms: i64) {
    let now = jiff::Timestamp::now().as_millisecond();
    let mut creds = Credentials::new("access".into(), Some("refresh".into()), Some(3600), 0);
    creds.expires_at_ms = u64::try_from(now + from_now_ms).expect("a time after 1970");
    FileStore::in_dir(vault).save(&creds).expect("save a login");
}

#[test]
fn whoami_reports_no_login_a_valid_one_and_an_expired_one() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    assert_eq!(ok(v, &["whoami"]), "not logged in");

    store_login(v, 3_600_000);
    let valid = ok(v, &["whoami"]);
    assert!(
        valid.starts_with("logged in; access token valid for another "),
        "{valid}"
    );

    store_login(v, -1_000);
    assert_eq!(
        ok(v, &["whoami"]),
        "logged in, but the access token has expired — run `sunrise login`"
    );
}

#[test]
fn logout_removes_the_stored_login_and_is_harmless_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    store_login(v, 3_600_000);
    assert!(v.join(CREDENTIALS_FILE).exists());

    assert_eq!(ok(v, &["logout"]), "Signed out.");
    assert!(
        !v.join(CREDENTIALS_FILE).exists(),
        "the token does not outlive the logout"
    );
    assert_eq!(ok(v, &["whoami"]), "not logged in");

    assert_eq!(ok(v, &["logout"]), "Signed out.");
}

/// The error a user hits first: it says which variable is missing and names
/// both of the ones to set, and nothing is written.
#[test]
fn login_without_its_configuration_names_what_to_set_and_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let both = format!("set {ENV_ISSUER} and {ENV_CLIENT_ID} first");

    for (set, missing) in [
        (None, ENV_ISSUER),
        (Some(ENV_CLIENT_ID), ENV_ISSUER),
        (Some(ENV_ISSUER), ENV_CLIENT_ID),
    ] {
        let mut login = sunrise(v, &["login"]);
        if let Some(name) = set {
            login.env(name, "configured");
        }
        let out = login.output().expect("run sunrise");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{set:?}: {stderr}");
        assert!(
            stderr.contains(&format!("{missing} is not set")),
            "{set:?}: {stderr}"
        );
        assert!(stderr.contains(&both), "{set:?}: {stderr}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("browser"),
            "{set:?}: it stops before the flow starts"
        );
    }
    assert!(!v.join(CREDENTIALS_FILE).exists());
}
