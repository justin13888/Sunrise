//! The snapshot record reader, [`Engine::apply_snapshot`], over bytes the
//! relay serves to a replica catching up on a stream (ADR-0059). The record's
//! magic prefix, its canonical CBOR map and its frontier are read before the
//! writer's signature is checked, so all of that runs on bytes nobody has
//! vouched for yet.
//!
//! One vault is stood up once, in memory, and reused for every input: a fresh
//! account on a fixed clock and a seeded generator, holding one task in the
//! Inbox stream, of which it wrote one snapshot record. That vault's device is
//! the only one whose key signed a record, and it signed that one.
//!
//! # What is asserted
//!
//! 1. **Every refusal is [`EngineError::Invalid`].** A record that is not
//!    the vault's own is malformed or unsigned, and the reader says so; it
//!    never reaches storage or the receive path, so no other error is
//!    reachable without a signature.
//! 2. **No forged record.** A record the reader accepts carries the setup
//!    record's fields one to ten, which are everything the signature covers
//!    and the signature itself. Fields above ten are not signed and are
//!    ignored, so they are not compared.
//! 3. **An accepted record adds nothing.** The vault's own prefix already
//!    reaches the frontier of the one record its device signed, so an accepted
//!    record is [`SnapshotApplied::Covered`] and writes nothing. Every input
//!    therefore meets the same vault.
//!
//! The seed is the setup record, byte for byte: the generator is seeded and
//! the clock fixed, so the vault this harness stands up writes the same record
//! every run.

#![no_main]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use ciborium::value::Value;
use libfuzzer_sys::fuzz_target;
use sunrise_core::{
    Clock, Command, Engine, EngineError, IdentitySeed, Keychain, Rng, SnapshotApplied,
};
use sunrise_crypto::VaultRootKey;
use sunrise_domain::{TaskDraft, INBOX_STREAM_BYTES};
use sunrise_storage::Db;

/// The vault root, the clock reading and the generator seed. Changing any of
/// them changes the setup record, and `fuzz/seeds/core_snapshot/` with it.
const ROOT: [u8; 32] = [0x5a; 32];
const NOW_MS: u64 = 1_780_000_000_000;
const RNG_SEED: u64 = 0x5eed_0000_0000_0810;

#[derive(Debug)]
struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        NOW_MS
    }
}

/// SplitMix64: deterministic, and enough for a harness whose keys protect
/// nothing.
#[derive(Debug)]
struct SeededRng(Mutex<u64>);

impl Rng for SeededRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        let mut state = self.0.lock().expect("the generator lock");
        for chunk in dest.chunks_mut(8) {
            *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }
}

struct Vault {
    engine: Engine,
    db: Db,
    /// The setup record's map, fields one to ten.
    signed: Vec<(Value, Value)>,
}

fn vault() -> Vault {
    let mut db = Db::open_memory(&VaultRootKey::from_bytes(ROOT)).expect("an in-memory vault");
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    let rng: Arc<dyn Rng> = Arc::new(SeededRng(Mutex::new(RNG_SEED)));
    let keychain = Keychain::open(
        &mut db,
        VaultRootKey::from_bytes(ROOT),
        clock.as_ref(),
        rng.as_ref(),
        &IdentitySeed::Own,
    )
    .expect("a fresh keychain");
    let engine = Engine::from_clock(clock, rng, Arc::new(keychain));
    engine
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "snapshot seed".into(),
                ..Default::default()
            }),
        )
        .expect("a task in the Inbox");
    let record = engine
        .write_stream_snapshot(&mut db, &INBOX_STREAM_BYTES)
        .expect("a snapshot write")
        .expect("a snapshot of a stream holding an op");
    let signed = signed_fields(&record).expect("the setup record is a canonical map");
    Vault { engine, db, signed }
}

/// Fields one to ten of a record's map, in key order, or `None` when the
/// bytes after the magic prefix are not a canonical CBOR map.
fn signed_fields(record: &[u8]) -> Option<Vec<(Value, Value)>> {
    let body = record.get(sunrise_cbor::MAGIC_LEN..)?;
    let Value::Map(fields) = sunrise_cbor::decode_canonical::<Value>(body).ok()? else {
        return None;
    };
    let key = |k: &Value| k.as_integer().and_then(|i| u64::try_from(i).ok());
    let mut out: Vec<(Value, Value)> = fields
        .into_iter()
        .filter(|(k, _)| key(k).is_some_and(|k| (1..=10).contains(&k)))
        .collect();
    out.sort_by_key(|(k, _)| key(k));
    Some(out)
}

thread_local! {
    static VAULT: RefCell<Vault> = RefCell::new(vault());
}

fuzz_target!(|data: &[u8]| {
    VAULT.with_borrow_mut(|v| {
        match v.engine.apply_snapshot(&mut v.db, data) {
            // 1.
            Err(EngineError::Invalid(_)) => {}
            Err(other) => panic!("a refusal that is not Invalid: {other:?}"),
            Ok((applied, events)) => {
                // 2.
                assert_eq!(
                    signed_fields(data).as_ref(),
                    Some(&v.signed),
                    "a record nobody signed was accepted"
                );
                // 3.
                assert_eq!(applied, SnapshotApplied::Covered);
                assert!(events.is_empty(), "a covered record emitted events");
            }
        }
    });
});
