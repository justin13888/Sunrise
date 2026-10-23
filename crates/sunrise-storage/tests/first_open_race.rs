//! Many threads keying their first vaults at the same moment (issue #521).
//!
//! `SQLite` marks itself initialised before `SQLCipher` has finished its own
//! initialisation, and a `PRAGMA key` in that window is refused with
//! "PRAGMA key requires a key of one or more characters". The window is the
//! first moments of a process, so this test lives in its own binary: nothing
//! else may touch `SQLite` first. Before the fix it failed in about 1 run in 70
//! on a loaded 16-core machine, with up to 20 of the 32 opens refused at once;
//! with `Db`'s cipher gate the threads wait for the first key instead.

use std::sync::{Arc, Barrier};

use sunrise_crypto::keys::VaultRootKey;
use sunrise_storage::Db;

#[test]
fn every_concurrent_first_open_is_keyed() {
    const THREADS: u8 = 32;
    let barrier = Arc::new(Barrier::new(usize::from(THREADS)));
    let opens: Vec<_> = (0..THREADS)
        .map(|seed| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                Db::open_memory(&VaultRootKey::from_bytes([seed; 32])).map(drop)
            })
        })
        .collect();
    let refused: Vec<String> = opens
        .into_iter()
        .filter_map(|open| open.join().expect("an open panicked").err())
        .map(|e| e.to_string())
        .collect();
    assert!(
        refused.is_empty(),
        "{} of {THREADS} first opens were refused, the first with: {}",
        refused.len(),
        refused[0]
    );
}
