//! Where limiter state lives.
//!
//! Two kinds of state, kept apart because only one of them can leave the
//! process:
//!
//! - **Rates** go through [`LimiterStore`], a trait, so a deployment running
//!   several relays behind one balancer can share them (#364). [`MemoryStore`]
//!   is the single-node implementation and the only one today.
//! - **Concurrency** — open SSE streams, open blob uploads — is a count of
//!   things this process is holding, released when it lets go of them. That is
//!   per-node by nature, so [`Gate`] and [`UploadLedger`] are plain types.

use super::bucket::{decide, Cell, Outcome, Quota};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Shared rate state, behind the seam a multi-node store would implement.
///
/// Async because the store a cluster shares is across a network; the
/// in-process one simply never awaits.
#[async_trait::async_trait]
pub trait LimiterStore: Send + Sync + std::fmt::Debug {
    /// Charge `cost` units to `key` under `quota`, or refuse.
    async fn take(&self, key: &str, quota: Quota, cost: u64, now_ms: u64) -> Outcome;

    /// Whether one unit would be admitted, charging nothing.
    async fn peek(&self, key: &str, quota: Quota, now_ms: u64) -> Outcome;
}

/// How many keys [`MemoryStore`] tracks before it starts forgetting.
///
/// Every key is a client address or a device, and a bucket that has refilled
/// is the same as no bucket, so this bounds memory during a flood from many
/// addresses rather than anything a real deployment holds at rest.
pub const MAX_KEYS: usize = 65_536;

/// The in-process [`LimiterStore`].
///
/// One lock over one map. A request holds it for one hash lookup and a few
/// integer operations, which is far below the cost of the request it guards.
#[derive(Debug, Default)]
pub struct MemoryStore {
    cells: Mutex<HashMap<String, Cell>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `decide` against `key` under the lock, and keep what it returns.
    ///
    /// A new key in a full map first evicts every idle cell, which changes no
    /// decision. If that frees nothing the request is admitted untracked:
    /// refusing it instead would let a flood from 65,536 addresses lock out
    /// every client the relay has not seen yet, which is a worse failure than
    /// the flood going briefly unlimited.
    fn apply(&self, key: &str, quota: Quota, cost: u64, now_ms: u64) -> Outcome {
        let mut cells = self.cells.lock();
        let current = cells.get(key).copied();
        // A peek at a key with no state is a full bucket, and storing that
        // would only make every peek a write.
        if cost == 0 && current.is_none() {
            return Outcome::untracked(quota.limit);
        }
        if current.is_none() && cells.len() >= MAX_KEYS {
            cells.retain(|_, cell| !cell.is_idle(now_ms));
            if cells.len() >= MAX_KEYS {
                return Outcome::untracked(quota.limit);
            }
        }
        let (outcome, next) = decide(quota, current, now_ms, cost);
        cells.insert(key.to_owned(), next);
        outcome
    }

    /// Keys currently tracked.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.cells.lock().len()
    }
}

#[async_trait::async_trait]
impl LimiterStore for MemoryStore {
    async fn take(&self, key: &str, quota: Quota, cost: u64, now_ms: u64) -> Outcome {
        self.apply(key, quota, cost.max(1), now_ms)
    }

    async fn peek(&self, key: &str, quota: Quota, now_ms: u64) -> Outcome {
        self.apply(key, quota, 0, now_ms)
    }
}

/// A cap on how many of something one key holds at once.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    held: Arc<Mutex<HashMap<String, u32>>>,
}

/// One unit of a [`Gate`], released when dropped.
#[derive(Debug)]
pub struct Permit {
    held: Arc<Mutex<HashMap<String, u32>>>,
    key: String,
}

impl Gate {
    /// A gate nobody holds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one unit for `key`, or `None` if it already holds `cap`.
    #[must_use]
    pub fn acquire(&self, key: &str, cap: u32) -> Option<Permit> {
        let mut held = self.held.lock();
        let count = held.entry(key.to_owned()).or_insert(0);
        if *count >= cap {
            return None;
        }
        *count += 1;
        Some(Permit {
            held: Arc::clone(&self.held),
            key: key.to_owned(),
        })
    }

    /// How many units `key` holds.
    #[must_use]
    pub fn held(&self, key: &str) -> u32 {
        self.held.lock().get(key).copied().unwrap_or(0)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut held = self.held.lock();
        if let Some(count) = held.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

/// How long an upload may go untouched before it stops counting as open.
///
/// An upload ends at `finalize`, and a client that crashes or is uninstalled
/// never sends one. Without an expiry those would count forever and an account
/// would eventually be locked out of attachments by uploads nobody is making.
pub const UPLOAD_IDLE_MS: u64 = 60 * 60 * 1000;

/// One account's open uploads: upload id to when it was last touched.
type OpenUploads = HashMap<[u8; 16], u64>;

/// The uploads each account has open, for the per-account cap.
#[derive(Debug, Clone, Default)]
pub struct UploadLedger {
    open: Arc<Mutex<HashMap<String, OpenUploads>>>,
}

impl UploadLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `upload` for `account`, or say how long until a slot frees.
    ///
    /// # Errors
    /// The milliseconds until the account's least recently touched upload
    /// expires, when it already holds `cap`.
    pub fn open(&self, account: &str, upload: [u8; 16], cap: u32, now_ms: u64) -> Result<(), u64> {
        let mut open = self.open.lock();
        let uploads = open.entry(account.to_owned()).or_default();
        uploads.retain(|_, touched| now_ms.saturating_sub(*touched) < UPLOAD_IDLE_MS);
        if uploads.len() >= cap as usize {
            let oldest = uploads.values().copied().min().unwrap_or(now_ms);
            return Err((oldest + UPLOAD_IDLE_MS).saturating_sub(now_ms));
        }
        uploads.insert(upload, now_ms);
        Ok(())
    }

    /// Mark `upload` as still in use.
    pub fn touch(&self, account: &str, upload: [u8; 16], now_ms: u64) {
        if let Some(touched) = self
            .open
            .lock()
            .get_mut(account)
            .and_then(|uploads| uploads.get_mut(&upload))
        {
            *touched = now_ms;
        }
    }

    /// `upload` is finished and no longer counts.
    pub fn close(&self, account: &str, upload: [u8; 16]) {
        let mut open = self.open.lock();
        if let Some(uploads) = open.get_mut(account) {
            uploads.remove(&upload);
            if uploads.is_empty() {
                open.remove(account);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_704_067_200_000;

    /// Two keys never share a bucket.
    #[tokio::test]
    async fn keys_are_independent() {
        let store = MemoryStore::new();
        let q = Quota::per_minute(1);
        assert!(store.take("a", q, 1, T0).await.allowed);
        assert!(!store.take("a", q, 1, T0).await.allowed);
        assert!(store.take("b", q, 1, T0).await.allowed);
    }

    /// A full map forgets idle keys before it stops tracking new ones, and a
    /// key it could not track is admitted rather than refused.
    #[tokio::test]
    async fn a_full_store_evicts_idle_keys_then_fails_open() {
        let store = MemoryStore::new();
        let q = Quota::per_minute(1);
        for i in 0..MAX_KEYS {
            store.take(&format!("k{i}"), q, 1, T0).await;
        }
        assert_eq!(store.len(), MAX_KEYS);

        let fresh = store.take("new", q, 1, T0).await;
        assert!(fresh.allowed, "an untracked key is admitted");
        assert_eq!(store.len(), MAX_KEYS, "and not stored");

        // A minute later every bucket is full again, so all are evictable.
        assert!(store.take("new", q, 1, T0 + 60_000).await.allowed);
        assert_eq!(store.len(), 1);
    }

    /// A permit is released when dropped, and only then.
    #[test]
    fn a_gate_caps_holders_until_a_permit_drops() {
        let gate = Gate::new();
        let a = gate.acquire("dev", 2).expect("first");
        let _b = gate.acquire("dev", 2).expect("second");
        assert!(gate.acquire("dev", 2).is_none());
        assert!(gate.acquire("other", 2).is_some(), "per key");
        drop(a);
        assert_eq!(gate.held("dev"), 1);
        assert!(gate.acquire("dev", 2).is_some());
    }

    /// An account at its cap is refused until an upload finishes, and is told
    /// how long until the stalest one stops counting.
    #[test]
    fn the_upload_cap_frees_on_close_or_after_idling() {
        let ledger = UploadLedger::new();
        ledger.open("acct", [1; 16], 2, T0).unwrap();
        ledger.open("acct", [2; 16], 2, T0 + 1_000).unwrap();
        assert_eq!(
            ledger.open("acct", [3; 16], 2, T0 + 2_000),
            Err(UPLOAD_IDLE_MS - 2_000)
        );
        assert!(ledger.open("other", [3; 16], 2, T0).is_ok(), "per account");

        ledger.close("acct", [1; 16]);
        assert!(ledger.open("acct", [3; 16], 2, T0 + 2_000).is_ok());

        // Touching keeps an upload open past the idle window.
        ledger.touch("acct", [2; 16], T0 + UPLOAD_IDLE_MS);
        assert!(ledger
            .open("acct", [4; 16], 2, T0 + UPLOAD_IDLE_MS + 1_000)
            .is_err());
        // ... and an untouched one stops counting.
        assert!(ledger
            .open("acct", [4; 16], 2, T0 + 2_000 + UPLOAD_IDLE_MS)
            .is_ok());
    }
}
