//! Where `sunrise undo` finds the step it reverses.
//!
//! [`sunrise_client_core::history::UndoHistory`] is a session's undo, and
//! every CLI command is a session of one. So the binary reads the history at
//! open, records each write into it, and writes it back before it exits; the
//! encoding is the client layer's ([`sunrise_client_core::history::persist`]),
//! and this module is only where it lives and how it is sealed.
//!
//! # Sealed, beside the vault
//!
//! A history is task content: every label names a task, and every inverse
//! carries the values it puts back. A vault directory copied on its own has to
//! stay ciphertext (`docs/07-clients/overview.md` §Multi-account), so the file
//! is XChaCha20-Poly1305 under a key derived from the vault's own root, which
//! never lives in the vault directory. A file that does not open — another
//! vault's, a truncated one, one from a build with another encoding — is a
//! history with nothing in it, never an error that stops the command.
//!
//! Like the in-memory history it is this device's alone: it is not synced, and
//! an undo is a new write that merges like any other.

use std::path::{Path, PathBuf};

use sunrise_client_core::history::{persist, UndoHistory};
use sunrise_core::Rng;
use sunrise_crypto::aead::{aead_open_xchacha, aead_seal_xchacha, AEAD_NONCE_LEN};
use sunrise_crypto::blake3_kdf::derive_key_32;

/// The history's file name inside the vault directory.
pub const UNDO_FILE: &str = "cli-undo";

/// The first bytes of the file, and its AEAD associated data: a file of
/// another layout fails to open rather than decoding as garbage.
const MAGIC: &[u8; 4] = b"SRU1";

/// The KDF context for the sealing key. Unique to this purpose.
const KEY_CONTEXT: &str = "sunrise.cli_undo.v1";

/// Where the history of the vault at `vault_dir` is kept.
#[must_use]
pub fn path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(UNDO_FILE)
}

/// What [`load`] found.
#[derive(Debug)]
pub struct Loaded {
    /// The history, empty when there was none or it did not open.
    pub history: UndoHistory,
    /// Why a file that was there was not read, for a note on stderr.
    pub discarded: Option<String>,
}

/// The vault's history, or an empty one.
#[must_use]
pub fn load(vault_dir: &Path, root: &[u8; 32]) -> Loaded {
    let empty = |why: Option<String>| Loaded {
        history: UndoHistory::new(),
        discarded: why,
    };
    let bytes = match std::fs::read(path(vault_dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return empty(None),
        Err(e) => return empty(Some(e.to_string())),
    };
    match open(&bytes, root).and_then(|json| persist::from_json(&json).map_err(|e| e.to_string())) {
        Ok(stacks) => Loaded {
            history: UndoHistory::from_stacks(stacks),
            discarded: None,
        },
        Err(why) => empty(Some(why)),
    }
}

/// Write `history` back, sealed. Returns how many steps were left out because
/// they hold a command the encoding does not carry.
///
/// # Errors
/// The file could not be written.
pub fn save(
    vault_dir: &Path,
    root: &[u8; 32],
    history: &UndoHistory,
    rng: &dyn Rng,
) -> std::io::Result<usize> {
    let (json, dropped) = persist::to_json(&history.snapshot())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let sealed = seal(json.as_bytes(), root, rng)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::private_file::write_private(&path(vault_dir), &sealed)?;
    Ok(dropped)
}

fn seal(plain: &[u8], root: &[u8; 32], rng: &dyn Rng) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let ct = aead_seal_xchacha(&derive_key_32(KEY_CONTEXT, root), &nonce, plain, MAGIC)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + nonce.len() + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(bytes: &[u8], root: &[u8; 32]) -> Result<String, String> {
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| "not an undo history file".to_owned())?;
    if rest.len() < AEAD_NONCE_LEN {
        return Err("truncated undo history".to_owned());
    }
    let (nonce, ct) = rest.split_at(AEAD_NONCE_LEN);
    let nonce: [u8; AEAD_NONCE_LEN] = nonce.try_into().map_err(|_| "bad nonce".to_owned())?;
    let plain = aead_open_xchacha(&derive_key_32(KEY_CONTEXT, root), &nonce, ct, MAGIC)
        .map_err(|e| e.to_string())?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::Command;
    use sunrise_id::{EntityKind, EntityRef};

    /// A counter, so the test is deterministic and two seals differ.
    #[derive(Debug, Default)]
    struct Counter(std::sync::atomic::AtomicU8);
    impl Rng for Counter {
        fn fill_bytes(&self, dest: &mut [u8]) {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            dest.fill(n);
        }
    }

    fn history_with_one_step() -> UndoHistory {
        let mut stacks = sunrise_client_core::history::UndoStacks::default();
        stacks.push(sunrise_client_core::UndoEntry {
            label: "complete \u{201c}Renew passport\u{201d}".into(),
            forward: vec![Command::CompleteTask(EntityRef::new(
                EntityKind::Task,
                [1; 16],
            ))],
            backward: vec![],
        });
        UndoHistory::from_stacks(stacks)
    }

    #[test]
    fn a_saved_history_reads_back_and_its_file_names_no_task() {
        let dir = tempfile::tempdir().unwrap();
        let root = [7u8; 32];
        save(
            dir.path(),
            &root,
            &history_with_one_step(),
            &Counter::default(),
        )
        .unwrap();

        let raw = std::fs::read(path(dir.path())).unwrap();
        assert!(
            !raw.windows(b"passport".len()).any(|w| w == b"passport"),
            "the file is ciphertext"
        );
        let loaded = load(dir.path(), &root);
        assert!(loaded.discarded.is_none());
        assert_eq!(
            loaded.history.labels().undo.as_deref(),
            Some("complete \u{201c}Renew passport\u{201d}")
        );
    }

    #[test]
    fn another_vaults_key_or_a_damaged_file_is_an_empty_history_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        save(
            dir.path(),
            &[7u8; 32],
            &history_with_one_step(),
            &Counter::default(),
        )
        .unwrap();
        let other = load(dir.path(), &[8u8; 32]);
        assert!(other.history.labels().undo.is_none());
        assert!(other.discarded.is_some());

        std::fs::write(path(dir.path()), b"SRU1short").unwrap();
        assert!(load(dir.path(), &[7u8; 32]).discarded.is_some());
    }

    #[test]
    fn no_file_is_an_empty_history_and_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(dir.path(), &[7u8; 32]);
        assert!(loaded.discarded.is_none());
        assert!(loaded.history.labels().undo.is_none());
    }
}
