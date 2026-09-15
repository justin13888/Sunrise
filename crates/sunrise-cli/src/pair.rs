//! `sunrise pair` — the four-command file exchange that adds a device.
//!
//! # The problem this shape solves
//!
//! Pairing is three messages since #105, because the account's signing key
//! stopped travelling and a sponsor cannot sign a `DeviceCert` for keys the
//! joiner has not minted yet. The Apple clients map those three onto the Noise
//! XX channel they already drive. **The CLI has no channel at all.** It paired
//! by file drop — one variable named a file to write, another named a file to
//! read — and that could carry exactly one message in one direction.
//!
//! So the CLI gets files, one per message, and a command per file:
//!
//! ```text
//!   sponsor                                joiner
//!   ------------------------------------   ------------------------------------
//!   sunrise pair offer --out offer.cbor
//!                                     -->  sunrise pair request \
//!                                            --offer offer.cbor \
//!                                            --out request.cbor
//!   sunrise pair issue \              <--
//!     --request request.cbor \
//!     --out grant.cbor
//!                                     -->  sunrise pair accept \
//!                                            --response grant.cbor
//! ```
//!
//! # The design decision, recorded
//!
//! The joiner's two steps are **two processes**, and there is state between
//! them: `D_S_priv` and `D_D_priv`, minted by `pair request` and needed by
//! `pair accept` to adopt the certificate signed over their public halves. They
//! cannot be re-minted at `accept` — the certificate names the first pair — and
//! they cannot be sent, which is the whole distinction from a design where the
//! sponsor mints the joiner's keypair and keeps the ability to impersonate it
//! forever.
//!
//! So `pair request` writes [`PENDING_FILE`] into the vault directory: the
//! offer it answered plus those two secrets, mode 0600. Three things about that
//! are deliberate:
//!
//! - **It is in the vault directory, not the keystore.** The keystore holds
//!   vault roots, which are long-lived and whose loss is unrecoverable; this is
//!   a few minutes of pairing state and is deleted the moment it is consumed.
//!   Filing it beside the roots would put a disposable secret in the one
//!   directory whose whole contract is "back this up".
//! - **It is `D_S_priv` in the clear.** There is nothing to encrypt it under:
//!   the vault root has not arrived yet — it is in the grant — and a key
//!   derived from something the machine already has would be theatre. Mode 0600
//!   is the same protection the vault root itself gets, and the exposure lasts
//!   from `request` to `accept` rather than forever.
//! - **A second `pair request` overwrites it.** A joiner that ran `request`,
//!   lost the file it produced, and ran it again gets a fresh pair of keys and
//!   a fresh pending file. The old ones are simply abandoned; nothing anywhere
//!   ever saw them, because the sponsor had not issued anything for them.
//!
//! # What happened to `SUNRISE_PAIRING_FILE`
//!
//! Gone, with `SUNRISE_EXPORT_PAIRING_FILE`. Neither can be kept. They were a
//! *one-shot* affordance: one file carrying the whole account including
//! `ID_S_priv`, written unprompted at startup by any vault and adopted
//! unprompted at startup by any other. Withholding `ID_S_priv` is the entire
//! point of #105, so the file they moved no longer exists in any form — and a
//! device cannot join with anything written before it minted the keys its
//! certificate names, so there is nothing for a startup-time variable to read.
//!
//! They were a development affordance rather than a contract, and this replaces
//! them with a real command that does the real protocol.
//!
//! # Why `offer` and `issue` do not use the ordinary open path
//!
//! `request` and `accept` must run *before* the joiner's vault exists. The
//! binary's usual sequence resolves a root and opens a core before it
//! dispatches, and on a fresh directory that mints a brand-new account — which
//! the joiner would then be unable to replace, because the identity a vault
//! belongs to is decided when it is created. So the whole `pair` family is
//! dispatched ahead of the open and each command opens exactly what it needs.

use std::path::{Path, PathBuf};

use sunrise_core::{Core, SystemRng};
use sunrise_pairing::{
    decode_pairing_grant, decode_pairing_offer, decode_pairing_request, PairingJoiner,
};

use crate::livesync;
use crate::private_file::write_private;
use crate::vault;

/// The joiner's minted device keys, between `pair request` and `pair accept`.
///
/// Inside the vault directory; see the module docs for why it is there, why it
/// is in the clear, and why overwriting it is safe.
pub const PENDING_FILE: &str = "pending-pairing";

/// One `sunrise pair` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairCommand {
    /// Message 1, on the sponsor. Public identity only.
    Offer {
        /// Where to write it.
        out: PathBuf,
    },
    /// Message 2, on the joiner. Mints `D_S`/`D_D` and keeps the secrets.
    Request {
        /// The sponsor's offer.
        offer: PathBuf,
        /// Where to write the request.
        out: PathBuf,
    },
    /// Message 3, on the sponsor. Issues the cert and hands over the vault.
    Issue {
        /// The joiner's request.
        request: PathBuf,
        /// Where to write the grant.
        out: PathBuf,
    },
    /// The joiner adopts the cert and opens its vault for the first time.
    Accept {
        /// The sponsor's grant.
        response: PathBuf,
    },
}

/// What a user typed wrong.
pub const USAGE: &str = "\
usage:
  sunrise pair offer    --out <file>                 (on the device with the vault)
  sunrise pair request  --offer <file> --out <file>  (on the device being added)
  sunrise pair issue    --request <file> --out <file>(on the device with the vault)
  sunrise pair accept   --response <file>            (on the device being added)

Run them in that order, moving each file to the other machine as it is written.
";

/// Parse `pair`'s arguments.
///
/// Hand-rolled like the rest of this binary's parsing, and flag-per-message
/// rather than positional: the four files are easy to mix up and
/// `--offer`/`--request`/`--response` say which one a command is being handed.
///
/// # Errors
/// [`USAGE`] for an unknown subcommand or a missing flag.
pub fn parse(args: &[String]) -> Result<PairCommand, String> {
    let sub = args.first().map(String::as_str).unwrap_or_default();
    let flag = |name: &str| -> Option<PathBuf> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    let need = |name: &str| flag(name).ok_or_else(|| format!("{name} is required\n\n{USAGE}"));
    match sub {
        "offer" => Ok(PairCommand::Offer {
            out: need("--out")?,
        }),
        "request" => Ok(PairCommand::Request {
            offer: need("--offer")?,
            out: need("--out")?,
        }),
        "issue" => Ok(PairCommand::Issue {
            request: need("--request")?,
            out: need("--out")?,
        }),
        "accept" => Ok(PairCommand::Accept {
            response: need("--response")?,
        }),
        "" => Err(USAGE.to_string()),
        other => Err(format!("unknown pair step {other:?}\n\n{USAGE}")),
    }
}

/// Run one `pair` command against the vault at `vault_dir`.
///
/// Returns the lines to print. Every one of them names a file, because moving
/// the file to the other machine is the step the user has to take next and a
/// command that did not say which file would be a command they have to guess
/// at.
///
/// # Errors
/// I/O failures, a message that does not decode, a sponsor that holds no
/// `ID_S_priv`, a grant whose cert does not name this device's keys, and — on
/// `accept` — a vault directory that already holds a vault.
pub async fn run(
    vault_dir: &Path,
    cmd: &PairCommand,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    match cmd {
        PairCommand::Offer { out } => offer(vault_dir, out).await,
        PairCommand::Request { offer, out } => request(vault_dir, offer, out),
        PairCommand::Issue { request, out } => issue(vault_dir, request, out).await,
        PairCommand::Accept { response } => accept(vault_dir, response).await,
    }
}

/// Open the sponsor's vault, offline.
///
/// No sync driver: these commands run for milliseconds and a relay session
/// opened for one would just churn. Pairing needs no network at all — the
/// transport is the user carrying files.
async fn open_sponsor(
    vault_dir: &Path,
) -> Result<std::sync::Arc<Core>, Box<dyn std::error::Error>> {
    let root = vault::resolve(vault_dir, &SystemRng)?;
    let (core, _log) = livesync::open_with_plan(
        vault_dir.to_path_buf(),
        env!("CARGO_PKG_VERSION"),
        root,
        &livesync::SyncPlan::default(),
        None,
    )
    .await?;
    Ok(core)
}

async fn offer(vault_dir: &Path, out: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let core = open_sponsor(vault_dir).await?;
    if !core.can_sponsor_pairing() {
        core.shutdown().await;
        return Err(
            "this vault was itself added by pairing, so it holds the account's public \
                    identity and no signing key; it cannot certify another device. Run this on \
                    the device the account was created on."
                .into(),
        );
    }
    let offer = core.export_pairing_offer()?;
    core.shutdown().await;
    // Not `write_private`: an offer carries no secret at all, and writing it
    // 0600 would imply otherwise to anyone reading the directory. Every other
    // file in this exchange does carry one.
    std::fs::write(out, offer.encode()?)?;
    tracing::info!(
        ev = "ui.pair.offer_written",
        result = "ok",
        "pairing offer written"
    );
    Ok(vec![format!(
        "wrote the pairing offer -> {}\nmove it to the device you are adding, then run:\n  \
         sunrise pair request --offer <that file> --out request.cbor",
        out.display()
    )])
}

fn request(
    vault_dir: &Path,
    offer_path: &Path,
    out: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if vault_dir.join("vault.db").exists() {
        return Err(format!(
            "{} already holds a vault; a device joins an account when its vault is created, so \
             pair into an empty directory (set SUNRISE_VAULT)",
            vault_dir.display()
        )
        .into());
    }
    let offer = decode_pairing_offer(&std::fs::read(offer_path)?)?;

    // The keys this device will be known by, from the injected CSPRNG the
    // workspace requires. `D_S_priv` is what every op this vault ever writes is
    // signed under, so nothing here reaches for a weaker source.
    let rng = SystemRng;
    let mut seed_s = [0u8; 32];
    let mut seed_d = [0u8; 32];
    {
        use sunrise_core::Rng as _;
        rng.fill_bytes(&mut seed_s);
        rng.fill_bytes(&mut seed_d);
    }
    let joiner = PairingJoiner::new(
        offer,
        "sunrise-cli".to_string(),
        std::env::consts::OS.to_string(),
        seed_s,
        seed_d,
    );
    // Pending state first, request second. A crash between them leaves a
    // pending file nobody asked about, which the next `pair request`
    // overwrites; the other order would publish a request whose keys this
    // machine no longer has, and the sponsor would issue a cert for a device
    // that can never sign anything.
    write_private(&vault_dir.join(PENDING_FILE), &joiner.encode()?)?;
    std::fs::write(out, joiner.request().encode()?)?;
    tracing::info!(
        ev = "ui.pair.request_written",
        result = "ok",
        "pairing request written"
    );
    Ok(vec![format!(
        "minted this device's keys and wrote the cert request -> {}\nmove it back to the device \
         with your vault, then run:\n  sunrise pair issue --request <that file> --out grant.cbor",
        out.display()
    )])
}

async fn issue(
    vault_dir: &Path,
    request_path: &Path,
    out: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let request = decode_pairing_request(&std::fs::read(request_path)?)?;
    let core = open_sponsor(vault_dir).await?;
    let grant = core.issue_pairing_grant(&request);
    core.shutdown().await;
    let grant = grant?;
    // Owner-only, and not at the process umask: this is the vault root and
    // every Stream key in the account, in the clear. It is the one file in the
    // exchange that is worth stealing — which is exactly the change from the
    // old affordance, where the *first* file was.
    write_private(out, &grant.encode()?)?;
    tracing::info!(
        ev = "ui.pair.grant_written",
        result = "ok",
        "pairing grant written"
    );
    Ok(vec![format!(
        "issued a certificate and wrote the grant -> {}\nthis file carries your vault key. Move \
         it to the device you are adding, then run:\n  sunrise pair accept --response <that file>",
        out.display()
    )])
}

async fn accept(
    vault_dir: &Path,
    response: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let pending_path = vault_dir.join(PENDING_FILE);
    let pending = std::fs::read(&pending_path).map_err(|e| {
        format!(
            "no pending pairing in {} ({e}); run `sunrise pair request` on this device first",
            vault_dir.display()
        )
    })?;
    let joiner = PairingJoiner::decode(&pending)?;
    let grant = decode_pairing_grant(&std::fs::read(response)?)?;
    // The check that makes a second sponsor useless: the cert must verify under
    // the `ID_S_pub` the *offer this device answered* named, and must name the
    // keys this device minted. A grant from anywhere else is refused here,
    // before a vault exists to be confused by it.
    let payload = joiner.accept(grant)?;

    // The account's root, not one minted locally. This is the only path in the
    // binary that keys a vault with a root it did not generate, and it is what
    // makes the new directory a second *device* rather than a second account.
    let root = payload.vault_root;
    vault::adopt(vault_dir, &vault::keystore_dir(), root, &SystemRng)?;

    let (core, _log) = livesync::open_with_plan(
        vault_dir.to_path_buf(),
        env!("CARGO_PKG_VERSION"),
        root,
        &livesync::SyncPlan::default(),
        Some(Box::new(payload)),
    )
    .await?;
    let device = crate::hex16_public(&core.device_id());
    core.shutdown().await;

    // Only now. The secrets in it have been consumed, and until the vault above
    // opened successfully they were the only copy of the keys the sponsor's
    // certificate names — deleting them earlier would strand a device that
    // holds a certificate it cannot sign under.
    let _ = std::fs::remove_file(&pending_path);
    tracing::info!(ev = "ui.pair.accepted", result = "ok", "pairing accepted");
    Ok(vec![format!(
        "this device joined the account as {device}\nrun `sunrise sync --once` against the \
         account's relay to pull its history"
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_parses_with_its_own_flag() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(String::from).collect() };
        assert_eq!(
            parse(&argv("offer --out a")).unwrap(),
            PairCommand::Offer { out: "a".into() }
        );
        assert_eq!(
            parse(&argv("request --offer a --out b")).unwrap(),
            PairCommand::Request {
                offer: "a".into(),
                out: "b".into()
            }
        );
        assert_eq!(
            parse(&argv("issue --request a --out b")).unwrap(),
            PairCommand::Issue {
                request: "a".into(),
                out: "b".into()
            }
        );
        assert_eq!(
            parse(&argv("accept --response a")).unwrap(),
            PairCommand::Accept {
                response: "a".into()
            }
        );
    }

    /// A missing flag names itself. The four files are easy to mix up and
    /// "usage:" on its own would not say which one was forgotten.
    #[test]
    fn a_missing_flag_says_which_one() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(String::from).collect() };
        assert!(parse(&argv("request --out b"))
            .unwrap_err()
            .contains("--offer"));
        assert!(parse(&argv("issue --out b"))
            .unwrap_err()
            .contains("--request"));
        assert!(parse(&argv("accept")).unwrap_err().contains("--response"));
        assert!(parse(&argv("offer")).unwrap_err().contains("--out"));
    }

    #[test]
    fn an_unknown_step_is_refused_rather_than_guessed() {
        let err = parse(&["exchange".to_string()]).unwrap_err();
        assert!(err.contains("exchange"));
        assert!(err.contains("usage:"));
        assert!(parse(&[]).unwrap_err().contains("usage:"));
    }
}
