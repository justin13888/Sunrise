//! `login`, `logout`, `whoami`, `bootstrap` and `recover`: the account.

use sunrise_cli::i18n::strings;
use sunrise_cli::{livesync, login};
use sunrise_core::SystemRng;

use super::{CmdResult, Ctx};

/// The four account verbs that run against an open vault.
pub(super) async fn run(ctx: &Ctx<'_>, sub: &str, rest: &[String]) -> CmdResult {
    let core = ctx.core;
    let vault_dir = ctx.vault_dir;
    match sub {
        "login" => {
            let cfg = login::LoginConfig::from_env().map_err(|e| {
                strings::login::needs_config(&e, login::ENV_ISSUER, login::ENV_CLIENT_ID)
            })?;
            let store = login::store_for(vault_dir);
            let device_id = login::device_id_hex(core);
            let mut announce = |line: &str| println!("{line}");
            println!("{}", strings::login::opening_browser());
            let creds =
                login::login(&cfg, &device_id, &store, core.now_ms(), &mut announce).await?;
            let secs = creds.expires_at_ms.saturating_sub(core.now_ms()) / 1000;
            println!(
                "{}",
                strings::login::signed_in(i64::try_from(secs).unwrap_or(i64::MAX))
            );
            Ok(())
        }
        "logout" => {
            login::logout(&login::store_for(vault_dir))?;
            println!("{}", strings::login::signed_out());
            Ok(())
        }
        "whoami" => {
            println!(
                "{}",
                login::status_line(&login::store_for(vault_dir), core.now_ms())
            );
            Ok(())
        }
        // The caller ADR-0021 opens by observing does not exist: "the
        // account-and-device bootstrap flow the crypto design depends on was
        // specified, served, and never invoked, and nothing detected that".
        //
        // Both calls are idempotent server-side, so running this twice adopts
        // the existing account rather than failing.
        _ => bootstrap(ctx, rest).await,
    }
}

/// `bootstrap [email]` — publish this vault to the relay and print the
/// recovery code once.
async fn bootstrap(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let core = ctx.core;
    let vault_dir = ctx.vault_dir;
    let store = login::store_for(vault_dir);
    let creds = sunrise_auth::store::CredentialStore::load(&store)?
        .ok_or_else(strings::bootstrap::not_signed_in)?;
    let url = std::env::var(livesync::ENV_SYNC_URL)
        .map_err(|_| strings::bootstrap::needs_relay(livesync::ENV_SYNC_URL))?;

    // The email is a fallback the relay reads only when the identity
    // provider emits no `email` claim, so the empty string rather than
    // an invented address when none was given.
    let sunrise_client_core::account::Bootstrapped {
        outcome,
        recovery_code,
    } = sunrise_client_core::account::bootstrap_account(
        core,
        &SystemRng,
        &url,
        &creds.access_token,
        rest.first().cloned().unwrap_or_default(),
        core.now_ms(),
        sunrise_cli::device_label(env!("CARGO_PKG_VERSION")),
    )
    .await?;

    // Recorded, not merely printed: `X-Sunrise-Device` names this row
    // on every later request, and the relay never sends the id again.
    // A client that dropped it would hold a signing key it could not
    // say whose it was.
    if let Err(e) = livesync::save_relay_device_id(vault_dir, &outcome.device_id) {
        return Err(strings::bootstrap::unrecorded(
            &outcome.device_id,
            &livesync::relay_device_path(vault_dir).display().to_string(),
            &e.to_string(),
        )
        .into());
    }

    println!(
        "{}",
        strings::bootstrap::ready(&outcome.identity_id, &outcome.email, &outcome.device_id)
    );
    // Printed only after the relay accepted the blob. A second
    // bootstrap that seals a *different* blob is refused with
    // `409 RECOVERY_BLOB_EXISTS` and never reaches this line, so a code
    // this command shows is always the code the stored blob opens.
    match recovery_code {
        Some(code) => print_recovery_code(&code),
        None => eprintln!(
            "{}",
            strings::general::note(&strings::bootstrap::paired_no_code())
        ),
    }
    Ok(())
}

/// `recover` — `docs/03-crypto/recovery.md` §Recovery flow, end to end.
///
/// The whole of it happens before there is a vault to open, which is why this
/// runs from `run`'s pre-open match rather than from `dispatch`. The sequence
/// and the reasons are in [`sunrise_cli::recover`]; what lives here is the part
/// that belongs to a terminal — where the words come from, and what is said
/// afterwards.
///
/// Progress goes to **stderr** and the outcome to stdout, the same split every
/// other subcommand makes: a script that runs this wants the identity id, not
/// the narration. The 24 words are never echoed back, printed, or logged.
pub(crate) async fn recover(dir: &std::path::Path, rest: &[String]) -> CmdResult {
    use std::io::Read as _;

    std::fs::create_dir_all(dir).ok();
    sunrise_cli::recover::require_empty(dir)?;

    let url = std::env::var(livesync::ENV_SYNC_URL)
        .map_err(|_| sunrise_cli::recover::RecoverError::NoRelay)?;

    // Read stdin only when there are no arguments, so a terminal session that
    // typed the words does not then block on a pipe nobody is writing to.
    let mut piped = String::new();
    if rest.is_empty() {
        std::io::stdin().read_to_string(&mut piped).ok();
    }
    let code = sunrise_cli::recover::code_from(rest, &piped)
        .ok_or(sunrise_cli::recover::RecoverError::NoCode)?;

    // stderr, because stdout is the contract a script reads.
    let mut announce = |line: &str| eprintln!("{line}");

    // The clock: there is no vault yet, so there is no `Core::now_ms` to ask,
    // and this value is only used to price the credential this call is about
    // to mint. `jiff` is already the CLI's time source everywhere else.
    let now_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0);
    let bearer = sunrise_cli::recover::step_up_bearer(dir, now_ms, &mut announce).await?;

    let identity = sunrise_cli::recover::restore_identity(&url, &bearer, &code).await?;
    drop(code);
    announce(&strings::recover::code_accepted());

    let done = sunrise_cli::recover::rejoin_account(
        dir,
        identity,
        &url,
        &bearer,
        env!("CARGO_PKG_VERSION"),
        &mut announce,
    )
    .await?;

    println!("{}", done.identity_id);
    eprintln!("{}", sunrise_cli::recover::aftercare(&done.identity_id));
    Ok(())
}

/// Show the recovery code, once, with what it is for and what losing it costs.
///
/// **On stdout and nowhere else.** It never becomes a `tracing` field: the type
/// implements no `Display` and no `tracing::Value`, its `Debug` prints only the
/// word count, and `sunrise-log`'s allowlist carries no field name for it — so
/// there is no spelling of a log call that could carry it even by accident. It
/// is also never written to a file: `docs/03-crypto/key-rotation.md` records
/// that a vault holding the only copy of `ID_D_priv` cannot be recovered by any
/// feature added later, and a copy this process saved would be a copy an
/// attacker who reaches the machine also has, while doing nothing for the user
/// who loses the machine.
fn print_recovery_code(code: &sunrise_crypto::bip39::RecoveryCode) {
    println!();
    println!("{}", strings::recover::code_heading());
    println!();
    // Six lines of four, because twenty-four words on one wrapped terminal
    // line is what a transcription error looks like before it happens.
    for row in code.reveal().split(' ').collect::<Vec<_>>().chunks(4) {
        println!("    {}", row.join(" "));
    }
    println!();
    println!("{}", strings::recover::code_footer());
}
