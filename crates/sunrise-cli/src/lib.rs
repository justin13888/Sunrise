//! Library surface of the Sunrise command-line client.
//!
//! Most subcommands live in the binary, where their stdout is their contract.
//! What is here is the part an integration test has to drive without a process
//! boundary: [`livesync`], the env→plan→running-sync-session sequence the
//! binary performs at startup, [`vault`], which decides what key a vault
//! directory is opened with, and the two families that run *before*
//! a vault is open — [`recover`], whose whole job happens before a vault
//! exists, and [`pair`], which runs before the vault is opened because the
//! usual path would create one the joiner could never replace, and whose
//! four-step exchange across two machines is worth driving end to end in one
//! process. [`i18n`] is here because both halves print prose: it is the
//! generated binding of the string catalog `i18n/en.toml`.
//!
//! The rest is the decisions a subcommand makes that are worth a unit test
//! without a vault: [`when`], which of the client layer's two shapes a typed
//! date is; [`prefs_text`], a preference value as typed and as printed; and
//! [`undo_file`], where `sunrise undo` finds the history an earlier process
//! wrote, sealed under the vault's own key; [`args`], the flags every
//! subcommand takes; [`zone`], which zone the reader is in and who said so;
//! [`json`], the `--json` shapes; and [`watch`], the `sync --watch` loop.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::single_match_else
)]

pub mod args;
pub mod i18n;
pub mod json;
pub mod livesync;
pub mod login;
pub mod pair;
pub mod prefs_text;
pub mod recover;
pub mod undo_file;
pub mod vault;
pub mod watch;
pub mod when;
pub mod zone;

/// How this CLI introduces itself to `POST /api/v1/devices`, on every
/// registration path: `sunrise bootstrap` and `sunrise recover` alike.
///
/// The nickname, the platform tag and the build. The keys and the vault id
/// come from the vault, in [`sunrise_client_core::account::device_identity`].
#[must_use]
pub fn device_label(app_version: &str) -> sunrise_client_core::account::DeviceLabel {
    sunrise_client_core::account::DeviceLabel {
        nickname: "sunrise-cli".to_owned(),
        platform: if cfg!(target_os = "macos") {
            "macos".to_owned()
        } else if cfg!(target_os = "windows") {
            "windows".to_owned()
        } else {
            "linux".to_owned()
        },
        app_version: Some(app_version.to_owned()),
    }
}
