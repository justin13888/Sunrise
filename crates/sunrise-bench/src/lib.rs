//! Benchmark support: a deterministic synthetic-vault generator plus a small
//! harness for opening a real file-backed SQLCipher vault wired to the
//! [`sunrise_core`] command/query [`Engine`].
//!
//! The generator drives the **real** `Engine::apply` command path with a fixed
//! [`Clock`] and a seeded [`Rng`], so a given seed always produces the same
//! task ids, timestamps, and titles. That determinism makes the output reusable
//! by future chaos / property work, not just Criterion benches.
//!
//! Nothing here is a hot production path; it only ever runs under `cargo bench`
//! and unit tests, so it opens tempdir-backed vaults directly rather than going
//! through [`sunrise_core::Core`]'s async lifecycle.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Product names (SQLCipher, ChaCha8, …) read fine unquoted in prose; the rest
// of the workspace relaxes this pedantic lint the same way.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use jiff::Timestamp;
use parking_lot::Mutex;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use sunrise_core::{Clock, Command, Engine, Keychain, Rng};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::TaskDraft;
use sunrise_storage::Db;
use tempfile::TempDir;

/// Fixed wall-clock instant used for every seeded vault (2025-06-15T22:13:20Z,
/// in ms since the Unix epoch). All synthetic scheduling is spread relative to
/// this instant so the generated data is stable across runs and machines.
pub const FIXED_NOW_MS: u64 = 1_750_000_000_000;

/// Milliseconds in a day.
const DAY_MS: i64 = 86_400_000;

/// A rare marker word injected into ~1% of seeded titles. Benches search for it
/// to exercise the FTS path against a realistically selective query.
pub const RARE_WORD: &str = "quokka";

/// Common words drawn (deterministically) into synthetic titles so full-text
/// search has realistic tokens to match.
const WORDS: &[&str] = &[
    "review", "email", "report", "meeting", "design", "plan", "write", "code", "test", "deploy",
    "budget", "call", "draft", "research", "update", "invoice", "schedule", "prepare", "follow",
    "sync",
];

/// Deterministic, fixed wall clock.
#[derive(Debug)]
struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

/// Deterministic, seedable RNG (`ChaCha8`). Interior-mutable so it satisfies the
/// `&self` [`Rng`] contract while producing a reproducible stream.
#[derive(Debug)]
struct SeededRng(Mutex<ChaCha8Rng>);

impl SeededRng {
    fn new(seed: u64) -> Self {
        Self(Mutex::new(ChaCha8Rng::seed_from_u64(seed)))
    }
}

impl Rng for SeededRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        self.0.lock().fill_bytes(dest);
    }
}

/// A ready-to-drive benchmark vault: a file-backed SQLCipher [`Db`] plus the
/// [`Engine`] that mutates and queries it. The backing tempdir is held for the
/// lifetime of the struct and cleaned up on drop.
#[derive(Debug)]
pub struct BenchVault {
    /// The command/query engine (fixed clock + seeded rng).
    pub engine: Engine,
    /// The open, file-backed SQLCipher database.
    pub db: Db,
    // Kept last so the DB is dropped before the directory is removed.
    _dir: TempDir,
}

/// Open a fresh, empty, file-backed SQLCipher vault in a tempdir, wired to an
/// [`Engine`] backed by a fixed clock and a `seed`-seeded RNG.
///
/// Two calls with the same `seed` mint the same device identity and, when driven
/// with the same commands, produce byte-for-byte identical op streams.
///
/// # Panics
/// Panics if the tempdir, database, or keychain cannot be created — benches and
/// tests treat those as unrecoverable setup failures.
#[must_use]
pub fn open_vault(seed: u64) -> BenchVault {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join("vault.db");
    let mut db = Db::open(&db_path, &VaultRootKey::from_bytes([7u8; 32])).expect("open vault db");

    let clock: Arc<dyn Clock> = Arc::new(FixedClock(FIXED_NOW_MS));
    let rng: Arc<dyn Rng> = Arc::new(SeededRng::new(seed));
    let keychain = Keychain::open(
        &mut db,
        VaultRootKey::from_bytes([7u8; 32]),
        clock.as_ref(),
        rng.as_ref(),
    )
    .expect("open keychain");
    let engine = Engine::from_clock(clock, rng, Arc::new(keychain));

    BenchVault {
        engine,
        db,
        _dir: dir,
    }
}

/// Build the `i`-th synthetic [`TaskDraft`] from a content RNG.
///
/// Titles carry two common searchable words; every 100th task additionally
/// carries [`RARE_WORD`] (≈1% selectivity for FTS). `scheduled_at` / `due_at`
/// are spread over ±30 days around [`FIXED_NOW_MS`], with `due_at` always kept
/// at or after `scheduled_at` to satisfy the domain invariant.
fn synth_draft(i: usize, rng: &mut ChaCha8Rng) -> TaskDraft {
    let word = |rng: &mut ChaCha8Rng| -> &'static str {
        let len = u32::try_from(WORDS.len()).expect("word list fits u32");
        WORDS[(rng.next_u32() % len) as usize]
    };
    let a = word(rng);
    let b = word(rng);
    let title = if i % 100 == 0 {
        format!("{a} {b} {RARE_WORD} {i}")
    } else {
        format!("{a} {b} task {i}")
    };

    let base = i64::try_from(FIXED_NOW_MS).expect("fixed now fits i64");
    // scheduled_at: present ~70% of the time, spread -30..=30 days.
    let scheduled_at = if rng.next_u32() % 10 < 7 {
        let off_days = i64::from(rng.next_u32() % 61) - 30;
        Some(ts(base + off_days * DAY_MS))
    } else {
        None
    };
    // due_at: present ~40% of the time. When both are set, keep due >= scheduled
    // by anchoring it 0..=14 days after the scheduled instant.
    let due_at = if rng.next_u32() % 10 < 4 {
        let extra_days = i64::from(rng.next_u32() % 15);
        let anchor = match scheduled_at {
            Some(s) => s.as_millisecond(),
            None => base + i64::from(rng.next_u32() % 61) * DAY_MS,
        };
        Some(ts(anchor + extra_days * DAY_MS))
    } else {
        None
    };

    let priority = if rng.next_u32() % 4 == 0 {
        Some(u8::try_from(rng.next_u32() % 5 + 1).expect("1..=5 fits u8"))
    } else {
        None
    };

    TaskDraft {
        title,
        scheduled_at: scheduled_at.map(Into::into),
        due_at: due_at.map(Into::into),
        priority,
        ..Default::default()
    }
}

/// A single representative draft for the submit micro-benchmark (deterministic,
/// no RNG). The engine still mints a fresh id per apply.
#[must_use]
pub fn submit_draft() -> TaskDraft {
    TaskDraft {
        title: "review budget report".to_string(),
        scheduled_at: Some(ts(i64::try_from(FIXED_NOW_MS).expect("fits") + DAY_MS).into()),
        ..Default::default()
    }
}

/// Seed `n` synthetic tasks into `vault` via the real `Engine::apply` command
/// path, marking ~10% of them done. Returns the created task ids (as canonical
/// strings) in creation order — the determinism unit is that two same-seed runs
/// return identical id vectors.
///
/// The content spread (titles, schedule offsets) is driven by a `content_seed`
/// separate from the vault's identity/op RNG, so callers can vary the payload
/// distribution without disturbing device-identity determinism.
///
/// # Panics
/// Panics if any command fails to apply (a seeding bug, not an expected error).
pub fn seed_tasks(vault: &mut BenchVault, n: usize, content_seed: u64) -> Vec<String> {
    let mut content_rng = ChaCha8Rng::seed_from_u64(content_seed);
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let draft = synth_draft(i, &mut content_rng);
        let res = vault
            .engine
            .apply(&mut vault.db, Command::CreateTask(draft))
            .expect("seed create task");
        let id = res.entity;
        ids.push(id.to_str());
        // ~10% completed so Today/queries see a realistic done ratio.
        if i % 10 == 0 {
            vault
                .engine
                .apply(&mut vault.db, Command::CompleteTask(id))
                .expect("seed complete task");
        }
    }
    ids
}

/// Convert epoch-ms to a jiff [`Timestamp`], clamping to the valid range.
fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).expect("timestamp in range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_100_is_deterministic() {
        let mut a = open_vault(42);
        let ids_a = seed_tasks(&mut a, 100, 7);
        let mut b = open_vault(42);
        let ids_b = seed_tasks(&mut b, 100, 7);
        assert_eq!(ids_a.len(), 100);
        assert_eq!(ids_a, ids_b, "same seed must reproduce the same task ids");
    }

    #[test]
    fn rare_word_selectivity_is_about_one_percent() {
        // Exactly every 100th title carries the rare word.
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let hits = (0..1000usize)
            .filter(|&i| synth_draft(i, &mut rng).title.contains(RARE_WORD))
            .count();
        assert_eq!(hits, 10, "≈1% of titles carry the rare FTS word");
    }
}
