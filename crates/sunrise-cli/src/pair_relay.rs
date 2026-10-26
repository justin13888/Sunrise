//! `sunrise pair join` and `sunrise pair add` — pairing over the relay, with
//! the six digits compared on both devices.
//!
//! ```text
//!   device being added                     device with the vault
//!   ------------------------------------   ------------------------------------
//!   sunrise pair join
//!     prints the pairing code         -->  sunrise pair add '<code>'
//!     both print the same six digits; each user confirms them
//!     opens the vault it was handed        issues its certificate
//! ```
//!
//! # The same path the apps take
//!
//! Nothing here is a second implementation of pairing. The script — which
//! transport, what each phase calls, what a late answer may change — is
//! [`sunrise_client_core::pairing_flow::PairingFlow`], run to its end by
//! [`sunrise_client_core::pairing::PairingRun`] against
//! [`sunrise_client_core::pairing::RelaySeam`], whose calls are the
//! [`sunrise_client_core::pairing::RelayPairing`] the UniFFI seam wraps for the
//! Apple apps. What is here is a terminal's: the arguments, where the relay
//! and the bearer come from, what is printed, how the digits are answered, and
//! — on the device being added — the vault directory the pairing ends in.
//!
//! # Which device shows the code
//!
//! The device **being added**, as in the apps: Noise XX starts with it, its
//! first message is what opens the relay session, and the code carries its
//! static key, which the device with the vault checks the handshake against
//! before any digits exist. So `join` comes first and prints the code, and
//! `add` takes it.
//!
//! # Answering the digits
//!
//! `--yes` accepts them; `--confirm-sas <digits>` accepts them only if they
//! are the digits this device shows (spaces and dashes ignored); otherwise
//! the command asks on stderr and reads the other device's digits from stdin,
//! so confirming means having looked at both screens. Anything that is not
//! the same six digits declines, which throws the pairing away on both
//! devices and sends nothing.
//!
//! # Output
//!
//! The code, the digits and the outcome go to stdout and everything else to
//! stderr, as every command splits them. With `--json` each is one JSON
//! object on its own line as it happens — `{"event":"code","code":…}`,
//! `{"event":"sas","sas":…}`, `{"event":"paired",…}` — because the code has
//! to be read while the command is still waiting for the other device.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use sunrise_client_core::pairing::{PairingRun, PairingSide, RelaySeam, RunStop};
use sunrise_client_core::pairing_flow::{
    grouped_sas, sas_matches, ManualReason, PairingFailureKind,
};
use sunrise_core::SystemRng;

use crate::i18n::strings;
use crate::{livesync, login, pair};

/// How this device answers the digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Ask on stderr and read the other device's digits from stdin.
    Ask,
    /// `--yes`: accept the digits shown.
    Yes,
    /// `--confirm-sas <digits>`: accept only these digits.
    Digits(String),
}

/// One relay pairing step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayCommand {
    /// On the device being added: show the code and join.
    Join {
        /// The account's address, hashed into the code (`--email`).
        email: String,
        /// How to answer the digits.
        confirm: Confirm,
    },
    /// On the device with the vault: pair with the device showing `code`.
    Add {
        /// The code as given; `-` reads it from stdin.
        code: String,
        /// How to answer the digits.
        confirm: Confirm,
    },
}

/// Whether `args` (what follows `pair`) is a relay step rather than a file
/// one.
#[must_use]
pub fn is_relay_step(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("join" | "add"))
}

/// Parse a relay step's arguments.
///
/// # Errors
/// The usage text for an unknown option, a missing value, both `--yes` and
/// `--confirm-sas`, or an `add` with no code.
pub fn parse(args: &[String]) -> Result<RelayCommand, String> {
    let sub = args.first().map(String::as_str).unwrap_or_default();
    let mut email = String::new();
    let mut yes = false;
    let mut digits = None;
    let mut code = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| strings::pair::flag_required(flag, &strings::pair::usage()))
        };
        match a.as_str() {
            "--yes" | "-y" => yes = true,
            "--confirm-sas" => digits = Some(value("--confirm-sas")?),
            "--email" if sub == "join" => email = value("--email")?,
            other
                if sub == "add" && code.is_none() && (other == "-" || !other.starts_with('-')) =>
            {
                code = Some(other.to_owned());
            }
            other => {
                return Err(strings::pair::unknown_flag(
                    &format!("{other:?}"),
                    &strings::pair::usage(),
                ))
            }
        }
    }
    let confirm = match (yes, digits) {
        (true, Some(_)) => return Err(strings::pair::both_answers()),
        (true, None) => Confirm::Yes,
        (false, Some(d)) => Confirm::Digits(d),
        (false, None) => Confirm::Ask,
    };
    match sub {
        "join" => Ok(RelayCommand::Join { email, confirm }),
        _ => Ok(RelayCommand::Add {
            code: code.ok_or_else(strings::pair::needs_code)?,
            confirm,
        }),
    }
}

/// Run a relay pairing step against the vault at `vault_dir`, printing as it
/// goes; `json` is the global `--json`.
///
/// # Errors
/// No relay or no sign-in, a vault that is there when it must not be (or not
/// when it must), digits that differ, and every way a pairing can fail, each
/// worded with what to do next.
pub async fn run(
    vault_dir: &Path,
    cmd: &RelayCommand,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::env::var(livesync::ENV_SYNC_URL)
        .map_err(|_| strings::pair::needs_relay(livesync::ENV_SYNC_URL))?;
    let relay = livesync::relay_origin(&raw)?;
    match cmd {
        RelayCommand::Join { email, confirm } => {
            join(vault_dir, &relay, email, confirm, json).await
        }
        RelayCommand::Add { code, confirm } => add(vault_dir, &relay, code, confirm, json).await,
    }
}

/// The device being added.
async fn join(
    vault_dir: &Path,
    relay: &str,
    email: &str,
    confirm: &Confirm,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Checked first, so nothing is asked of the relay for a pairing that could
    // not end in a vault: the account a vault belongs to is fixed when it is
    // created.
    if vault_dir.join("vault.db").exists() {
        return Err(strings::pair::not_empty(&vault_dir.display().to_string()).into());
    }
    let bearer = joiner_bearer(vault_dir).await?;
    let label = crate::device_label(env!("CARGO_PKG_VERSION"));
    let seam = RelaySeam::new(
        relay,
        &bearer,
        email,
        PairingSide::Joiner {
            label: label.clone(),
            rng: Arc::new(SystemRng),
        },
    );
    let mut run = PairingRun::add_this_device(seam);
    drive(&mut run, relay, confirm, json).await?;
    let payload = run
        .seam_mut()
        .take_joined()
        .ok_or("the pairing finished without handing this device a vault")?;

    let core = pair::adopt(vault_dir, payload).await?;
    let device = sunrise_client_core::ids::hex_id(&core.device_id());
    // Registered here because the relay and the bearer are already in hand; a
    // refusal is a note rather than a failure, because the pairing itself is
    // done and `sunrise bootstrap` registers a paired device on its own.
    let relay_device =
        match sunrise_client_core::account::register_relay_device(&core, relay, &bearer, label)
            .await
        {
            Ok(id) => match livesync::save_relay_device_id(vault_dir, &id) {
                Ok(()) => {
                    note(&strings::pair::registered(&id));
                    Some(id)
                }
                Err(e) => {
                    note(&strings::bootstrap::unrecorded(
                        &id,
                        &livesync::relay_device_path(vault_dir).display().to_string(),
                        &e.to_string(),
                    ));
                    None
                }
            },
            Err(e) => {
                note(&strings::pair::unregistered(&e.to_string()));
                None
            }
        };
    core.shutdown().await;
    tracing::info!(
        ev = "ui.pair.relay_joined",
        result = "ok",
        "paired over the relay"
    );
    if json {
        emit(&json!({"event": "paired", "device": device, "relay_device": relay_device}));
    } else {
        say(&strings::pair::joined(&device));
    }
    Ok(())
}

/// The device with the vault.
async fn add(
    vault_dir: &Path,
    relay: &str,
    code: &str,
    confirm: &Confirm,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let code = if code == "-" {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    } else {
        code.to_owned()
    };
    if code.trim().is_empty() {
        return Err(strings::pair::needs_code().into());
    }
    // Checked before the open, which would otherwise mint a vault here.
    if !vault_dir.join("vault.db").exists() {
        return Err(strings::rebuild::no_vault(&vault_dir.display().to_string()).into());
    }
    let core = pair::open_sponsor(vault_dir).await?;
    let ran = sponsor(vault_dir, relay, &code, confirm, json, &core).await;
    core.shutdown().await;
    ran?;
    tracing::info!(
        ev = "ui.pair.relay_sponsored",
        result = "ok",
        "sponsored a device over the relay"
    );
    if json {
        emit(&json!({"event": "paired"}));
    } else {
        say(&strings::pair::sponsored());
    }
    Ok(())
}

async fn sponsor(
    vault_dir: &Path,
    relay: &str,
    code: &str,
    confirm: &Confirm,
    json: bool,
    core: &Arc<sunrise_core::Core>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !core.can_sponsor_pairing() {
        return Err(strings::pair::cannot_sponsor().into());
    }
    let bearer = stored_bearer(vault_dir, core.now_ms())
        .ok_or_else(|| strings::pair::needs_sign_in(livesync::ENV_SYNC_TOKEN))?;
    let seam = RelaySeam::new(relay, &bearer, "", PairingSide::Sponsor(Arc::clone(core)));
    let mut run = PairingRun::add_another_device(seam, code);
    drive(&mut run, relay, confirm, json).await
}

/// Run the pairing to its outcome, printing the code and the digits and
/// answering the digits as `confirm` says. Ctrl-C cancels it, leaving nothing
/// open on the relay.
async fn drive(
    run: &mut PairingRun<RelaySeam>,
    relay: &str,
    confirm: &Confirm,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    note(&strings::pair::waiting());
    loop {
        let stop = tokio::select! {
            stop = run.next() => Some(stop),
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(stop) = stop else {
            run.cancel().await;
            return Err(stopped("cancelled", strings::pair::cancelled()));
        };
        match stop {
            RunStop::ShowCode(code) => {
                if json {
                    emit(&json!({"event": "code", "code": code}));
                } else {
                    say(&strings::pair::show_code());
                    say(&code);
                }
                note(&strings::pair::waiting());
            }
            RunStop::Compare(sas) => {
                if json {
                    emit(&json!({"event": "sas", "sas": sas}));
                } else {
                    say(&strings::pair::compare(&grouped_sas(&sas)));
                }
                let matched = answer(confirm, &sas).await;
                run.answer(matched);
            }
            RunStop::Paired => return Ok(()),
            RunStop::Mismatch => return Err(stopped("mismatch", strings::pair::mismatch())),
            RunStop::Failed { message, kind } => {
                let why = match kind {
                    PairingFailureKind::RelayCode => strings::pair::failed_code(&message),
                    PairingFailureKind::General => message,
                };
                return Err(stopped("failed", why));
            }
            RunStop::NoRelay { reason, detail } => {
                let why = match (reason, detail) {
                    (ManualReason::RelayRefused, Some(detail)) => {
                        strings::pair::relay_refused(&detail)
                    }
                    _ => strings::pair::relay_did_not_answer(relay, livesync::ENV_SYNC_URL),
                };
                return Err(stopped("no_relay", why));
            }
        }
    }
}

/// This user's answer to the digits.
async fn answer(confirm: &Confirm, sas: &str) -> bool {
    match confirm {
        Confirm::Yes => true,
        Confirm::Digits(typed) => sas_matches(typed, sas),
        Confirm::Ask => {
            #[allow(clippy::print_stderr)]
            {
                eprint!("{}", strings::pair::prompt());
            }
            let _ = std::io::stderr().flush();
            // Off the runtime's workers: a person takes as long as they take,
            // and the relay session waits for them rather than for a poll.
            let typed = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map(|_| line)
            })
            .await;
            matches!(typed, Ok(Ok(line)) if sas_matches(&line, sas))
        }
    }
}

/// A pairing that ended without pairing: logged by outcome, returned as what
/// to tell the user.
fn stopped(result: &'static str, why: String) -> Box<dyn std::error::Error> {
    tracing::info!(
        ev = "ui.pair.relay_stopped",
        result,
        "pairing over the relay stopped"
    );
    why.into()
}

/// The bearer this vault's commands present: `SUNRISE_SYNC_TOKEN`, else a
/// stored sign-in that has not expired.
fn stored_bearer(vault_dir: &Path, now_ms: u64) -> Option<String> {
    bearer_of(
        livesync::SyncEnv::from_process_env().with_stored(&login::store_for(vault_dir), now_ms),
    )
}

/// [`stored_bearer`]'s choice over explicit inputs, so it is tested without
/// the process environment: the env token trimmed, if it is not blank, else
/// the stored token as stored.
fn bearer_of(env: livesync::SyncEnv) -> Option<String> {
    env.token
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .or(env.stored_token)
}

/// The bearer on the device being added, which has no vault for
/// `sunrise login` to run against: as [`stored_bearer`], else a sign-in run
/// now and kept in the vault directory for the commands after it.
async fn joiner_bearer(vault_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // No vault, so no `Core::now_ms`; this prices a stored credential and
    // nothing else, as `sunrise recover` does.
    let now_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0);
    if let Some(bearer) = stored_bearer(vault_dir, now_ms) {
        return Ok(bearer);
    }
    let cfg = login::LoginConfig::from_env().map_err(|e| {
        strings::pair::joiner_needs_sign_in(
            &e,
            login::ENV_ISSUER,
            login::ENV_CLIENT_ID,
            livesync::ENV_SYNC_TOKEN,
        )
    })?;
    note(&strings::pair::signing_in());
    let mut announce = |line: &str| note(line);
    let creds = login::login(
        &cfg,
        "",
        &login::store_for(vault_dir),
        now_ms,
        &mut announce,
    )
    .await?;
    Ok(creds.access_token)
}

/// A line for stdout, flushed: the other device is waiting on it.
fn say(line: &str) {
    #[allow(clippy::print_stdout)]
    {
        println!("{line}");
    }
    let _ = std::io::stdout().flush();
}

/// One `--json` event on its own line.
fn emit(event: &serde_json::Value) {
    say(&event.to_string());
}

/// A line for the person, on stderr.
fn note(line: &str) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn both_relay_steps_parse_with_their_answers() {
        assert_eq!(
            parse(&argv("join")).unwrap(),
            RelayCommand::Join {
                email: String::new(),
                confirm: Confirm::Ask
            }
        );
        assert_eq!(
            parse(&argv("join --email a@example.com --yes")).unwrap(),
            RelayCommand::Join {
                email: "a@example.com".into(),
                confirm: Confirm::Yes
            }
        );
        assert_eq!(
            parse(&argv("add {\"c\":1} --confirm-sas 123456")).unwrap(),
            RelayCommand::Add {
                code: "{\"c\":1}".into(),
                confirm: Confirm::Digits("123456".into())
            }
        );
        assert_eq!(
            parse(&argv("add - -y")).unwrap(),
            RelayCommand::Add {
                code: "-".into(),
                confirm: Confirm::Yes
            }
        );
        assert!(is_relay_step(&argv("join")) && is_relay_step(&argv("add x")));
        assert!(!is_relay_step(&argv("offer --out a")) && !is_relay_step(&[]));
    }

    #[test]
    fn a_relay_step_refuses_what_it_cannot_mean() {
        assert!(parse(&argv("add")).unwrap_err().contains("pair add"));
        assert!(parse(&argv("join --yes --confirm-sas 1")).is_err());
        assert!(parse(&argv("join --confirm-sas"))
            .unwrap_err()
            .contains("--confirm-sas"));
        assert!(parse(&argv("join --out x")).unwrap_err().contains("--out"));
        assert!(
            parse(&argv("add code --email a")).is_err(),
            "the code carries no address on the device with the vault"
        );
        assert!(parse(&argv("add one two")).is_err(), "one code");
    }

    fn env_with(token: Option<&str>, stored: Option<&str>) -> livesync::SyncEnv {
        livesync::SyncEnv {
            token: token.map(str::to_owned),
            stored_token: stored.map(str::to_owned),
            ..livesync::SyncEnv::default()
        }
    }

    /// `SUNRISE_SYNC_TOKEN` beats a stored login, trimmed, as it does in
    /// [`livesync::plan_from_env`].
    #[test]
    fn the_env_token_beats_a_stored_login() {
        assert_eq!(
            bearer_of(env_with(Some("  from-env  "), Some("stored-access"))).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            bearer_of(env_with(Some("from-env"), None)).as_deref(),
            Some("from-env")
        );
    }

    /// A blank export is unset, so the stored login behind it is used; with
    /// neither there is no bearer, which is what asks for a sign-in.
    #[test]
    fn a_blank_env_token_falls_back_to_the_stored_login() {
        for blank in ["", "   "] {
            assert_eq!(
                bearer_of(env_with(Some(blank), Some("stored-access"))).as_deref(),
                Some("stored-access"),
                "{blank:?}"
            );
            assert_eq!(bearer_of(env_with(Some(blank), None)), None, "{blank:?}");
        }
        assert_eq!(bearer_of(env_with(None, None)), None);
    }

    /// Pinned as it behaves today, which is not as
    /// [`livesync::plan_from_env`] behaves: the stored token is taken as
    /// stored, neither trimmed nor dropped when blank, where the plan drops a
    /// blank one. A change that routes this through the plan's precedence
    /// flips these assertions, and should mean to.
    #[test]
    fn a_stored_token_is_taken_as_stored_even_when_blank() {
        assert_eq!(
            bearer_of(env_with(None, Some("  stored-access  "))).as_deref(),
            Some("  stored-access  ")
        );
        for blank in ["", "   "] {
            assert_eq!(
                bearer_of(env_with(None, Some(blank))).as_deref(),
                Some(blank),
                "{blank:?}"
            );
        }
    }
}
