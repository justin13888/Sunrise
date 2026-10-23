//! Many threads opening their first encrypted stores at the same moment
//! (issue #521).
//!
//! `SQLite` marks itself initialised before `SQLCipher` has finished its own
//! initialisation, and a `PRAGMA key` in that window is refused with
//! "PRAGMA key requires a key of one or more characters". The window is the
//! first moments of a process, so this test lives in its own binary: nothing
//! else may touch `SQLite` first. The store's keying waits on
//! `sunrise_storage::ensure_cipher_ready` instead.

use std::sync::{Arc, Barrier};

use sunrise_server::store::{DbKey, DEFAULT_BUSY_TIMEOUT};
use sunrise_server::Store;

#[test]
fn every_concurrent_first_keyed_open_succeeds() {
    const THREADS: u8 = 16;
    let dir = tempfile::tempdir().expect("a temp dir");
    let barrier = Arc::new(Barrier::new(usize::from(THREADS)));
    let opens: Vec<_> = (0..THREADS)
        .map(|seed| {
            let barrier = Arc::clone(&barrier);
            let path = dir.path().join(format!("relay-{seed}.db"));
            std::thread::spawn(move || {
                let key = DbKey::from_bytes([seed; 32]);
                barrier.wait();
                Store::open_keyed(Some(&path), DEFAULT_BUSY_TIMEOUT, Some(&key)).map(drop)
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
        "{} of {THREADS} first keyed opens were refused, the first with: {}",
        refused.len(),
        refused[0]
    );
}
