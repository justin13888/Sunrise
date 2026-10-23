//! `sunrise` — the command-line client.
//!
//! Every subcommand is one-shot: open the vault, do the thing, print the
//! result, exit. That makes the whole stack — `Core::open`, the vault lock,
//! SQLCipher persistence, the capture parser, the query path, the sync driver
//! — drivable and testable **headlessly**, with no client and no terminal,
//! which is what `tests/cli.rs` does against the real binary.
//!
//! ```text
//! sunrise capture 'Renew passport #travel ^next saturday !1 ~1h'
//! sunrise today
//! sunrise next
//! SUNRISE_SYNC_URL=http://127.0.0.1:8443 sunrise sync --once
//! ```
//!
//! Deliberately hand-rolled rather than pulling in an arg parser: the surface
//! is positional subcommands with a few flags each, and the crate has no
//! dependency that is not already earning its place. This file opens the
//! vault; the subcommands are in `cmd`, one module per family.
//!
//! # stdout is the contract; logs go to a file
//!
//! A CLI's stdout is what a script reads, so nothing else may be written
//! there: `sunrise export trends json | jq` has to work. Human-facing notes
//! (an unresolved `@context`, say) go to stderr, and every `tracing` record
//! goes to `$XDG_STATE_HOME/sunrise/log/sunrise-cli.ndjson` (default
//! `~/.local/state/sunrise/log/…`, override with `SUNRISE_LOG_FILE`), which
//! [`sunrise_log::init_file`] caps at 16 MiB with one kept generation.
//!
//! If that file cannot be opened the binary runs **with no logging at all**.
//! Falling back to stderr would put NDJSON in the middle of a user's output,
//! and a missing log is the better of the two failures.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines
)]

mod cmd;

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_cli::i18n::strings;
use sunrise_cli::{livesync, login, pair, undo_file, vault};
use sunrise_core::SystemRng;

/// Returns [`std::process::ExitCode`] rather than a `Result`, because the
/// `Termination` impl for a `Result` prints the error's **`Debug`** form.
/// `VaultError` spends most of its length explaining what a user should do
/// next, and `RootMissing { id: "07a5…", keystore: "/var/…" }` is not that
/// explanation — it is the struct the explanation was written on. Printing
/// `Display` ourselves is the whole difference between a typed error and a
/// typed error a user can act on, and it drops the `Error: "…"` quoting that
/// the default handler put around every message this binary already had.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // First statement in the process. `init_file` is fallible and its failure
    // is deliberately discarded: an unwritable state directory must not stop
    // the command, and the alternative destination — stderr — would put
    // NDJSON in the middle of the user's output.
    let _ = sunrise_log::init_file("sunrise-cli");
    // Same shape as `sunrise-server`'s `srv.start`: the protocol versions are
    // reported once per process rather than on every record.
    let proto = sunrise_log::ProtoVersions::new(WIRE_PROTO_V, DOC_SCHEMA_V, CRYPTO_SUITE_V);
    tracing::info!(
        ev = "ui.start",
        app_v = env!("CARGO_PKG_VERSION"),
        wire_v = u64::from(proto.wire),
        doc_v = u64::from(proto.doc),
        crypto_v = u64::from(proto.crypto),
        "sunrise-cli starting"
    );
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = args.first() else {
        #[allow(clippy::print_stdout)]
        {
            print!("{}", strings::usage::text());
        }
        return std::process::ExitCode::SUCCESS;
    };
    match run(sub, &args[1..]).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // stderr, because stdout is the contract a script reads.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("{}", strings::general::error(&e.to_string()));
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn vault_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SUNRISE_VAULT") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".sunrise")
        .join("vault")
}

/// `vaults` — every vault this machine holds a key for, marking the open one.
///
/// Multi-account is only a capability if a user can find their accounts. The
/// macOS app answers this with a picker in Settings; the CLI answers it with a
/// listing, because a `SUNRISE_VAULT` nobody can enumerate is a feature you
/// have to already know about to use.
///
/// A vault opened only with `SUNRISE_VAULT_ROOT` never touches the keystore
/// and so is deliberately absent: this lists what is *keyed here*, which is
/// the question a user asking "what have I got" is really asking.
fn vaults() {
    #![allow(clippy::print_stdout)]
    let keystore = vault::keystore_dir();
    let current = vault_dir();
    let rows = vault::registered(&keystore);
    if rows.is_empty() {
        // stdout stays empty so `sunrise vaults | wc -l` is honest; the reason
        // there is nothing to list is a note for the human.
        #[allow(clippy::print_stderr)]
        {
            eprintln!("{}", strings::vaults::none(&keystore.display().to_string()));
        }
        return;
    }
    for v in rows {
        let mark = if v.path == current { "  [current]" } else { "" };
        println!("{}  {}{mark}", v.id, v.path.display());
    }
}

/// Dispatch a subcommand. Returns `Ok` on success; the process exit code
/// is non-zero only on a real failure, so scripts can branch on it.
async fn run(sub: &str, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // A CLI's whole job is writing to stdout; the workspace-wide ban on
    // print_stdout exists to keep it out of *library* code.
    #![allow(clippy::print_stdout)]
    match sub {
        "help" | "--help" | "-h" => {
            print!("{}", strings::usage::text());
            return Ok(());
        }
        "--version" | "-V" => {
            println!("sunrise {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // Answered without opening anything. It is the subcommand a user
        // reaches for when a vault will *not* open, so it must not need one to
        // have opened — the same reason the macOS registry lives in
        // `UserDefaults` rather than in the vault.
        "vaults" => {
            vaults();
            return Ok(());
        }
        // Dispatched **before** the open, and that is not an optimisation.
        // `pair request` and `pair accept` run on a device whose vault does not
        // exist yet, and the sequence below would create one — with a brand-new
        // account identity the joiner could never replace, because the identity
        // a vault belongs to is decided when it is created. Each `pair` step
        // opens exactly what it needs, which for two of them is nothing.
        "pair" => {
            let cmd = pair::parse(rest)?;
            let dir = vault_dir();
            std::fs::create_dir_all(&dir).ok();
            for line in pair::run(&dir, &cmd).await? {
                println!("{line}");
            }
            return Ok(());
        }
        // Answered without opening anything for the same reason, in the other
        // direction: this subcommand *creates* the vault, from an identity it
        // has not fetched yet. Falling through to the open below would mint a
        // root and an identity of its own first, and the account a vault
        // belongs to is decided when it is created — so the recovery would
        // then have nothing left to join.
        "recover" => {
            cmd::recover(&vault_dir(), rest).await?;
            return Ok(());
        }
        _ => {}
    }

    let dir = vault_dir();
    std::fs::create_dir_all(&dir).ok();
    let dir_for_store = dir.clone();
    // This vault's own root — see `sunrise_cli::vault`. Minted on the first
    // open of a directory and kept in the keystore from then on, so two
    // `SUNRISE_VAULT` values are two accounts and not two folders sharing one
    // key. `SystemRng` is the injected CSPRNG the workspace requires; nothing
    // here reaches for `rand` directly.
    let root = vault::resolve(&dir, &SystemRng)?;
    // Opened offline first, so the vault is available to price a stored
    // token against the core's clock rather than the host's — which the
    // workspace lint bans reading directly.
    //
    // `None`: this open never adopts a pairing. Joining an account happens in
    // `sunrise pair accept`, which is dispatched above and opens its own core
    // with the material a completed exchange produced. There used to be an
    // environment variable read here instead, and it cannot survive #105 —
    // see [`sunrise_cli::pair`].
    let mut env = livesync::SyncEnv::from_process_env();
    let (core, open_log) = livesync::open_with_plan(
        dir,
        env!("CARGO_PKG_VERSION"),
        root,
        &livesync::SyncPlan::default(),
        None,
    )
    .await?;

    // Then the real plan. Almost nothing starts a driver: opening one for a
    // command that exits milliseconds later would just churn the relay.
    //
    // `attachment` is the second verb that needs one, and it needs one for the
    // reason `sunrise_core::blob_fetch` explains — the transport belongs to the
    // driver task, so a download with no driver is a request nothing can
    // service. The core answers that case rather than hanging on it, so a
    // `sunrise attachment get` with no `SUNRISE_SYNC_URL` reports it instead of
    // waiting; an attachment whose bytes are already here still works offline,
    // because locality is checked before the relay is.
    if !matches!(sub, "sync" | "attachment") {
        env.url = None;
    }
    let plan = livesync::plan_from_env(
        &env.with_stored(&login::store_for(&dir_for_store), core.now_ms())
            .with_relay_device(&dir_for_store),
    );
    // The startup banner names the files it touched, which is what the
    // two-replica walkthrough needs to see. stderr, because stdout is the
    // contract a script reads — and not a log record, because those strings
    // carry filesystem paths (`livesync::apply_plan`, "Two outputs").
    #[allow(clippy::print_stderr)]
    for line in open_log
        .into_iter()
        .chain(livesync::apply_plan(&core, &plan))
    {
        eprintln!("{line}");
    }

    // The undo history this vault's earlier commands left (`sunrise undo`).
    let loaded = undo_file::load(&dir_for_store, &root);
    #[allow(clippy::print_stderr)]
    if let Some(why) = &loaded.discarded {
        eprintln!("{}", strings::general::note(&strings::undo::discarded(why)));
    }
    let ctx = cmd::Ctx {
        core: &core,
        vault_dir: &dir_for_store,
        history: loaded.history,
        dirty: std::cell::Cell::new(false),
    };
    let result = cmd::dispatch(&ctx, sub, rest).await;
    // Written back only when something was recorded, so a read never writes
    // a file into the vault directory.
    if ctx.dirty.get() {
        #[allow(clippy::print_stderr)]
        if let Err(e) = undo_file::save(&dir_for_store, &root, &ctx.history, &SystemRng) {
            eprintln!(
                "{}",
                strings::general::note(&strings::undo::unsaved(&e.to_string()))
            );
        }
    }
    core.shutdown().await;
    result
}
