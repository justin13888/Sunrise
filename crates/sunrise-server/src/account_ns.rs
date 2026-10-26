//! The account namespace: the one 16-byte key every per-account store is filed
//! under.
//!
//! [`account_key`] is that key. A session's relay channels, the relay log's
//! `relay_frames.account_h`, account erasure, the maintenance pass and the blob
//! tree's per-account directories all take it, so they agree by construction:
//! erasure deletes exactly what a session wrote because both call this
//! function. [`crate::logging::account_h`] is the log-side handle, a different
//! and shorter digest that never keys anything stored.
//!
//! `relay_log` re-exports it as `relay_log::account_key`, the path the e2e
//! tests import.

/// The namespace for an account: the first 16 bytes of `BLAKE3(account_id)`.
///
/// The account id never travels to another account, and the hash is what keys
/// a channel, so a stream is addressable only by someone who already knows
/// whose it is. A hash rather than the id, so no backend ever holds an account
/// id in a path, an object key or a relay row.
#[must_use]
pub fn account_key(account_id: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(account_id.as_bytes()).as_bytes()[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::account_key;
    use crate::relay_log::DurableCaps;
    use crate::Store;

    /// Ids of every shape an account id could take: empty, ASCII, a UUID,
    /// multi-byte UTF-8, and lengths either side of BLAKE3's 64-byte block and
    /// 1024-byte chunk boundaries.
    fn ids() -> Vec<String> {
        let mut ids: Vec<String> = [
            "",
            "a",
            "alice",
            "acct_01HZX3K9Q2V7",
            "0b9f6c1e-3a4d-4e8b-9c2f-7d1a5e6b8c90",
            "ünïcødé-账户-🔑",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for n in [63, 64, 65, 1023, 1024, 1025, 4096] {
            ids.push("x".repeat(n));
        }
        // A deterministic spread of arbitrary bytes-as-chars, so the check
        // covers ids nobody chose by hand without making the test random.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..256 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = usize::try_from(seed % 97).unwrap();
            let id: String = (0..len)
                .map(|i| {
                    // Below the surrogate range, so every value is a char.
                    let c = seed.rotate_left(u32::try_from(i % 64).unwrap()) % 0xd800;
                    char::from_u32(u32::try_from(c).unwrap()).unwrap()
                })
                .collect();
            ids.push(id);
        }
        ids
    }

    /// The key is the first half of `BLAKE3(account_id)` and nothing else.
    ///
    /// Relay frames and blob directories already on disk are filed under this
    /// construction, so it cannot change without orphaning them: erasure and
    /// the maintenance pass would stop finding what earlier sessions wrote.
    #[test]
    fn the_key_is_the_first_half_of_the_ids_blake3() {
        for id in ids() {
            let digest = blake3::hash(id.as_bytes());
            assert_eq!(account_key(&id)[..], digest.as_bytes()[..16], "id {id:?}");
        }
        // One fixed answer, so a change to the hash itself is caught too.
        assert_eq!(
            hex::encode(account_key("alice")),
            "71b278f3dc434447fc620500e47b6a80"
        );
    }

    /// Distinct ids land in distinct namespaces, so the key is not satisfied
    /// by a function that ignores its input.
    #[test]
    fn distinct_ids_get_distinct_namespaces() {
        let mut seen = std::collections::HashMap::new();
        for id in ids() {
            if let Some(other) = seen.insert(account_key(&id), id.clone()) {
                assert_eq!(other, id, "two ids share a namespace");
            }
        }
        assert!(seen.len() > 200, "the id spread is not degenerate");
    }

    /// A frame written under the key a session's `account` field holds is
    /// deleted by erasing that account by id, and another account's frame is
    /// not.
    #[test]
    fn the_key_a_session_holds_keys_erasure() {
        const STREAM: [u8; 16] = [0x11; 16];
        let store = Store::open(None).unwrap();
        let (alice, bob) = ("alice", "bob");
        for who in [alice, bob] {
            store
                .relay_append(
                    (account_key(who), STREAM),
                    b"frame",
                    &[],
                    0,
                    None,
                    1,
                    1_000,
                    DurableCaps::default(),
                )
                .unwrap();
        }

        store.erase_account(alice).unwrap();

        assert_eq!(store.relay_len((account_key(alice), STREAM)).unwrap(), 0);
        assert_eq!(
            store.relay_len((account_key(bob), STREAM)).unwrap(),
            1,
            "another account's log is untouched"
        );
    }
}
