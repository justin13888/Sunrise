//! The rendezvous's state: live sessions, their typed codes, and the
//! pair-attempt and code-lookup ledgers, behind the routes in `pairing.rs`.
//!
//! Apart from the handlers because it changes for a different reason: this is
//! the policy (who may open, read and look up what, and how often), driven by
//! a test clock, and the handlers are its HTTP shape.
//!
//! [`Rendezvous`] is the seam. [`MemoryRendezvous`] is the single-binary
//! relay's, a map behind one mutex; `store::postgres::PgRendezvous` is a relay
//! running as several nodes over Postgres, where the two devices of one pairing
//! may reach different nodes and the attempt limits must hold across all of
//! them. Both take every decision through [`Session`], [`free_code`] and
//! `sunrise_pairing::AttemptWindow`, so the policy is written once.

use super::{PairRole, MESSAGES_PER_ROLE, SESSION_TTL_MS};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use sunrise_pairing::{AttemptLimit, AttemptWindow};

/// Live sessions the relay holds at once, across every account. Past it a new
/// session is refused as rate-limited until one expires, rather than letting
/// memory grow with whoever opens the most.
pub const MAX_SESSIONS: usize = 4096;

/// Keys an attempt ledger may hold before idle ones are swept.
const MAX_LEDGER_KEYS: usize = 65_536;

/// How many six-digit codes there are.
const CODE_SPACE: u32 = 1_000_000;

/// Why the rendezvous refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No live session under that id for this account: never opened, expired,
    /// aborted, overflowed, or another account's.
    Gone,
    /// A pair attempt over the limit, or the relay's session table is full.
    Limited {
        /// Milliseconds until a retry would be admitted, at least 1.
        retry_after_ms: u64,
    },
    /// The shared store did not answer, and nothing was changed. The cause is
    /// logged where it happened; the client retries.
    Unavailable,
}

impl Refusal {
    /// The `result` label `sunrise_pairing_total` counts the refusal under,
    /// or `None` for a store failure, which `srv.store.failed` reports.
    pub(super) const fn label(self) -> Option<&'static str> {
        match self {
            Self::Gone => Some("gone"),
            Self::Limited { .. } => Some("rate_limited"),
            Self::Unavailable => None,
        }
    }
}

/// A message the rendezvous accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// Messages the sender has now sent in this session.
    pub count: u32,
    /// Whether this message opened the session.
    pub opened: bool,
    /// Whether this was a resend of a message already held, and so changed
    /// nothing.
    pub replayed: bool,
    /// When the session is dropped.
    pub expires_at_ms: u64,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
    /// The session's typed code, to the new device until the existing device
    /// has joined.
    pub code: Option<u32>,
}

/// What a code lookup found, and what it swept on the way.
#[derive(Debug)]
pub struct Resolved {
    /// The session's pair id and expiry, or the refusal.
    pub result: Result<([u8; 16], u64), Refusal>,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
}

/// What a receive found, and what it swept on the way.
#[derive(Debug)]
pub struct Received {
    /// The other side's messages from the cursor on and the session's expiry,
    /// or the refusal.
    pub result: Result<(Vec<Vec<u8>>, u64), Refusal>,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
}

/// Where pairing sessions and the pair-attempt ledgers live.
///
/// Every method takes `now_ms`, so the policy is driven by a test clock rather
/// than by sleeping. Every method also drops the sessions it finds expired,
/// and reports how many.
#[async_trait::async_trait]
pub trait Rendezvous: Send + Sync + std::fmt::Debug {
    /// Buffer `message` as `role`'s message number `index`.
    ///
    /// `address` is the client's rate-limit key, or `None` where limits are
    /// off; with `None` neither attempt limit is charged. Resending the message
    /// already held at `index` is acknowledged and changes nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when no live session of this account is named and the
    /// message cannot open one — it is not the new device's first — and when
    /// `index` is past the next free slot, past the protocol's three, or holds
    /// a different message; those three also drop the session.
    /// [`Refusal::Limited`] when opening a session is over an attempt limit or
    /// the session table is full. [`Refusal::Unavailable`] when the store did
    /// not answer.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        account_id: &str,
        address: Option<&str>,
        pair_id: [u8; 16],
        role: PairRole,
        index: usize,
        message: Vec<u8>,
        now_ms: u64,
    ) -> Result<Sent, Refusal>;

    /// The session `account_id` holds under `code`, charging one lookup to
    /// `address` and the account where `address` is given.
    ///
    /// The result is [`Refusal::Limited`] when the lookup is over either
    /// limit, and [`Refusal::Gone`] when no live session of this account holds
    /// `code`.
    async fn resolve(
        &self,
        account_id: &str,
        address: Option<&str>,
        code: u32,
        now_ms: u64,
    ) -> Resolved;

    /// What `role`'s peer has sent, from index `after` on.
    async fn receive(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        role: PairRole,
        after: usize,
        now_ms: u64,
    ) -> Received;

    /// Drop the session, if this account holds it. Whether one was dropped.
    ///
    /// # Errors
    ///
    /// [`Refusal::Unavailable`] when the store did not answer.
    async fn abort(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        now_ms: u64,
    ) -> Result<bool, Refusal>;
}

/// One live session, wherever it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The account whose bearer opened it.
    pub account_id: String,
    /// When it is dropped whatever happens.
    pub expires_at_ms: u64,
    /// The typed code, until the existing device sends its first message.
    pub code: Option<u32>,
    /// Indexed by `PairRole::index`: what each side has sent.
    pub sent: [Vec<Vec<u8>>; 2],
}

impl Session {
    /// A session `account_id` opens at `now_ms` under `code`, holding nothing
    /// yet.
    #[must_use]
    pub fn open(account_id: &str, code: u32, now_ms: u64) -> Self {
        Self {
            account_id: account_id.to_owned(),
            expires_at_ms: now_ms + SESSION_TTL_MS,
            code: Some(code),
            sent: [Vec::new(), Vec::new()],
        }
    }

    /// What `role` has sent.
    #[must_use]
    pub fn sent_by(&self, role: PairRole) -> &[Vec<u8>] {
        &self.sent[role.index()]
    }

    /// Take `message` as `role`'s message number `index`: `Some(true)` for a
    /// resend of the message already held there, which changes nothing,
    /// `Some(false)` for a new one, now held, and `None` when the session must
    /// be dropped.
    pub fn take(&mut self, role: PairRole, index: usize, message: Vec<u8>) -> Option<bool> {
        let mine = &mut self.sent[role.index()];
        match mine.get(index).map(|held| *held == message) {
            // A retry of a send whose answer was lost: acknowledged, not
            // buffered twice.
            Some(true) => return Some(true),
            // The index is taken by a different message, or is past the next
            // free one, or past the protocol's three: the two sides no longer
            // agree on where the exchange is, and nothing either sends now
            // could decrypt. The spec drops the session on overflow; the same
            // answer serves the rest.
            Some(false) => return None,
            None if index != mine.len() || index >= MESSAGES_PER_ROLE => return None,
            None => mine.push(message),
        }
        if role == PairRole::ExistingDevice {
            // The existing device has joined: the code has done its work, and
            // a second lookup must not hand the session to anyone else.
            self.code = None;
        }
        Some(false)
    }

    /// The answer to `role`'s accepted send.
    #[must_use]
    pub fn answer(&self, role: PairRole, opened: bool, replayed: bool, expired: usize) -> Sent {
        Sent {
            count: u32::try_from(self.sent_by(role).len()).unwrap_or(u32::MAX),
            opened,
            replayed,
            expires_at_ms: self.expires_at_ms,
            expired,
            code: self.code.filter(|_| role == PairRole::NewDevice),
        }
    }

    /// What `role`'s peer has sent, from index `after` on.
    #[must_use]
    pub fn for_peer_of(&self, role: PairRole, after: usize) -> Vec<Vec<u8>> {
        self.sent_by(role.peer())
            .get(after..)
            .unwrap_or_default()
            .to_vec()
    }
}

/// A code for a new session of an account whose live sessions hold `taken`:
/// drawn from the random `pair_id`, then stepped past every taken code. An
/// account holds far fewer than `CODE_SPACE` sessions, so the step ends.
///
/// The taken codes are collected once rather than rescanned per step: the
/// `pair_id` is the client's to choose, so a client can make every one of its
/// sessions draw the same code, and a rescan per step would make each open
/// cost the square of the table.
#[must_use]
pub fn free_code<S: std::hash::BuildHasher>(taken: &HashSet<u32, S>, pair_id: [u8; 16]) -> u32 {
    let [a, b, c, d, ..] = pair_id;
    let mut code = u32::from_be_bytes([a, b, c, d]) % CODE_SPACE;
    while taken.contains(&code) {
        code = (code + 1) % CODE_SPACE;
    }
    code
}

/// The in-memory rendezvous: live sessions and the pair-attempt ledgers.
///
/// Memory, not the store: a session lives five minutes and holds ciphertext
/// nobody can use after it, so a restart losing every one costs the users in
/// the middle of a pairing one rescan, and persisting it would put pairing
/// traffic on disk for no reader. A relay running as several nodes shares one
/// in Postgres instead, so either half of a pairing may reach any node.
#[derive(Debug, Clone, Default)]
pub struct MemoryRendezvous {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<[u8; 16], Session>,
    by_account: HashMap<String, AttemptWindow>,
    by_address: HashMap<String, AttemptWindow>,
    lookups_by_account: HashMap<String, AttemptWindow>,
    lookups_by_address: HashMap<String, AttemptWindow>,
}

impl MemoryRendezvous {
    /// An empty rendezvous.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sessions currently live, expired ones included until the next call.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().sessions.len()
    }
}

#[async_trait::async_trait]
impl Rendezvous for MemoryRendezvous {
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        account_id: &str,
        address: Option<&str>,
        pair_id: [u8; 16],
        role: PairRole,
        index: usize,
        message: Vec<u8>,
        now_ms: u64,
    ) -> Result<Sent, Refusal> {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        let opened = !inner.sessions.contains_key(&pair_id);
        if opened {
            // Only the new device's first message opens a session. A later
            // message naming a session that is gone — expired, say — must not
            // open a fresh one and leave the sender waiting on a peer that
            // will never come.
            if role != PairRole::NewDevice || index != 0 {
                return Err(Refusal::Gone);
            }
            if inner.sessions.len() >= MAX_SESSIONS {
                return Err(Refusal::Limited {
                    retry_after_ms: inner.next_expiry(now_ms),
                });
            }
            if let Some(address) = address {
                inner.admit(account_id, address, now_ms)?;
            }
            let taken: HashSet<u32> = inner
                .sessions
                .values()
                .filter(|s| s.account_id == account_id)
                .filter_map(|s| s.code)
                .collect();
            let code = free_code(&taken, pair_id);
            inner
                .sessions
                .insert(pair_id, Session::open(account_id, code, now_ms));
        }
        let Some(session) = inner
            .sessions
            .get_mut(&pair_id)
            .filter(|s| s.account_id == account_id)
        else {
            return Err(Refusal::Gone);
        };
        let Some(replayed) = session.take(role, index, message) else {
            inner.sessions.remove(&pair_id);
            return Err(Refusal::Gone);
        };
        Ok(session.answer(role, opened, replayed, expired))
    }

    async fn resolve(
        &self,
        account_id: &str,
        address: Option<&str>,
        code: u32,
        now_ms: u64,
    ) -> Resolved {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        if let Some(address) = address {
            if let Err(refusal) = inner.admit_lookup(account_id, address, now_ms) {
                return Resolved {
                    result: Err(refusal),
                    expired,
                };
            }
        }
        let result = inner
            .sessions
            .iter()
            .find(|(_, s)| s.account_id == account_id && s.code == Some(code))
            .map(|(id, s)| (*id, s.expires_at_ms))
            .ok_or(Refusal::Gone);
        Resolved { result, expired }
    }

    async fn receive(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        role: PairRole,
        after: usize,
        now_ms: u64,
    ) -> Received {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        let result = inner
            .sessions
            .get(&pair_id)
            .filter(|s| s.account_id == account_id)
            .map(|s| (s.for_peer_of(role, after), s.expires_at_ms))
            .ok_or(Refusal::Gone);
        Received { result, expired }
    }

    async fn abort(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        now_ms: u64,
    ) -> Result<bool, Refusal> {
        let mut inner = self.inner.lock();
        inner.reap(now_ms);
        let ours = inner
            .sessions
            .get(&pair_id)
            .is_some_and(|s| s.account_id == account_id);
        if ours {
            inner.sessions.remove(&pair_id);
        }
        Ok(ours)
    }
}

impl Inner {
    /// Drop every expired session; how many went.
    fn reap(&mut self, now_ms: u64) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| s.expires_at_ms > now_ms);
        before - self.sessions.len()
    }

    /// Milliseconds until the soonest session expires, at least 1.
    fn next_expiry(&self, now_ms: u64) -> u64 {
        self.sessions
            .values()
            .map(|s| s.expires_at_ms.saturating_sub(now_ms))
            .min()
            .unwrap_or(SESSION_TTL_MS)
            .max(1)
    }

    /// Charge one pair attempt to both ledgers, or refuse it with the longer
    /// of the two waits. Neither is charged on a refusal.
    fn admit(&mut self, account_id: &str, address: &str, now_ms: u64) -> Result<(), Refusal> {
        admit(
            &mut self.by_account,
            &mut self.by_address,
            account_id,
            address,
            now_ms,
        )
    }

    /// [`Self::admit`], for a code lookup: the same limits, in ledgers of
    /// their own, so looking codes up and opening sessions do not share one
    /// budget.
    fn admit_lookup(
        &mut self,
        account_id: &str,
        address: &str,
        now_ms: u64,
    ) -> Result<(), Refusal> {
        admit(
            &mut self.lookups_by_account,
            &mut self.lookups_by_address,
            account_id,
            address,
            now_ms,
        )
    }
}

/// Admit one attempt at `now_ms` into both windows, or refuse it with the
/// longer of the two waits, leaving both as they were.
///
/// # Errors
///
/// [`Refusal::Limited`] when either window is full.
pub fn admit_both(
    account: &mut AttemptWindow,
    client: &mut AttemptWindow,
    now_ms: u64,
) -> Result<(), Refusal> {
    let (mut a, mut c) = (account.clone(), client.clone());
    let waits = [
        a.try_admit(AttemptLimit::PER_ACCOUNT, now_ms).err(),
        c.try_admit(AttemptLimit::PER_ADDRESS, now_ms).err(),
    ];
    if let Some(retry_after_ms) = waits.into_iter().flatten().max() {
        return Err(Refusal::Limited { retry_after_ms });
    }
    *account = a;
    *client = c;
    Ok(())
}

/// Charge one attempt to both ledgers, or refuse it with the longer of the
/// two waits. Neither is charged on a refusal.
fn admit(
    by_account: &mut HashMap<String, AttemptWindow>,
    by_address: &mut HashMap<String, AttemptWindow>,
    account_id: &str,
    address: &str,
    now_ms: u64,
) -> Result<(), Refusal> {
    sweep(by_account, now_ms);
    sweep(by_address, now_ms);
    let mut account = by_account.get(account_id).cloned().unwrap_or_default();
    let mut client = by_address.get(address).cloned().unwrap_or_default();
    admit_both(&mut account, &mut client, now_ms)?;
    by_account.insert(account_id.to_owned(), account);
    by_address.insert(address.to_owned(), client);
    Ok(())
}

/// Forget the ledgers of keys that have made no attempt for a day, once the
/// map is large enough to be worth the walk.
fn sweep(ledgers: &mut HashMap<String, AttemptWindow>, now_ms: u64) {
    if ledgers.len() >= MAX_LEDGER_KEYS {
        ledgers.retain(|_, w| !w.is_idle(now_ms));
    }
}

/// What every [`Rendezvous`] owes, run against the in-memory one here and the
/// Postgres one in `store::postgres`.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{PairRole, Refusal, Rendezvous, SESSION_TTL_MS};

    const T0: u64 = 1_800_000_000_000;
    const ALICE: &str = "alice";
    const BOB: &str = "bob";
    const NEW: PairRole = PairRole::NewDevice;
    const EXISTING: PairRole = PairRole::ExistingDevice;

    fn id(seed: u8) -> [u8; 16] {
        [seed; 16]
    }

    /// The contract, over `one` and `two`: two handles on one rendezvous,
    /// the same one for a single process, two nodes for a shared store. Every
    /// step alternates between them, so a store that kept anything per handle
    /// fails here.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run(one: &dyn Rendezvous, two: &dyn Rendezvous) {
        // A pairing, its two halves on different handles.
        let opened = one
            .send(ALICE, None, id(1), NEW, 0, b"m1".to_vec(), T0)
            .await
            .unwrap();
        assert!(opened.opened && !opened.replayed);
        assert_eq!(opened.count, 1);
        assert_eq!(opened.expires_at_ms, T0 + SESSION_TTL_MS);
        let code = opened.code.expect("the opening answer carries a code");
        let resolved = two.resolve(ALICE, None, code, T0 + 1).await;
        assert_eq!(resolved.result, Ok((id(1), T0 + SESSION_TTL_MS)));
        assert_eq!(
            two.resolve(BOB, None, code, T0 + 1).await.result,
            Err(Refusal::Gone),
            "a code resolves only for its own account"
        );
        let got = two.receive(ALICE, id(1), EXISTING, 0, T0 + 2).await;
        assert_eq!(got.result, Ok((vec![b"m1".to_vec()], T0 + SESSION_TTL_MS)));
        let joined = two
            .send(ALICE, None, id(1), EXISTING, 0, b"m2".to_vec(), T0 + 3)
            .await
            .unwrap();
        assert!(!joined.opened);
        assert_eq!(joined.code, None, "the existing device is never shown it");
        assert_eq!(
            one.resolve(ALICE, None, code, T0 + 4).await.result,
            Err(Refusal::Gone),
            "spent once the existing device joined"
        );
        let third = one
            .send(ALICE, None, id(1), NEW, 1, b"m3".to_vec(), T0 + 5)
            .await
            .unwrap();
        assert_eq!((third.count, third.code), (2, None));
        assert_eq!(
            two.receive(ALICE, id(1), EXISTING, 1, T0 + 6)
                .await
                .result
                .unwrap()
                .0,
            vec![b"m3".to_vec()]
        );
        assert_eq!(
            one.receive(ALICE, id(1), NEW, 0, T0 + 6)
                .await
                .result
                .unwrap()
                .0,
            vec![b"m2".to_vec()]
        );

        // A resend is acknowledged and changes nothing; a different message
        // at a held index drops the session.
        let again = two
            .send(ALICE, None, id(1), NEW, 1, b"m3".to_vec(), T0 + 7)
            .await
            .unwrap();
        assert!(again.replayed && again.count == 2);
        assert_eq!(
            one.send(ALICE, None, id(1), NEW, 1, b"other".to_vec(), T0 + 8)
                .await,
            Err(Refusal::Gone)
        );
        assert_eq!(
            two.receive(ALICE, id(1), EXISTING, 0, T0 + 9).await.result,
            Err(Refusal::Gone)
        );

        // Skipping ahead, and a fourth message, each drop the session.
        one.send(ALICE, None, id(2), NEW, 0, b"a".to_vec(), T0)
            .await
            .unwrap();
        assert_eq!(
            two.send(ALICE, None, id(2), NEW, 2, b"c".to_vec(), T0)
                .await,
            Err(Refusal::Gone)
        );
        assert_eq!(
            one.send(ALICE, None, id(2), NEW, 1, b"b".to_vec(), T0)
                .await,
            Err(Refusal::Gone),
            "dropped by the skip"
        );
        for (i, m) in [b"a", b"b", b"c"].into_iter().enumerate() {
            one.send(ALICE, None, id(3), NEW, i, m.to_vec(), T0)
                .await
                .unwrap();
        }
        assert_eq!(
            two.send(ALICE, None, id(3), NEW, 3, b"d".to_vec(), T0)
                .await,
            Err(Refusal::Gone)
        );

        // Only the new device's first message opens one; another account sees
        // it as gone, and cannot abort it.
        assert_eq!(
            one.send(ALICE, None, id(4), EXISTING, 0, b"x".to_vec(), T0)
                .await,
            Err(Refusal::Gone)
        );
        assert_eq!(
            one.send(ALICE, None, id(4), NEW, 1, b"x".to_vec(), T0)
                .await,
            Err(Refusal::Gone)
        );
        one.send(ALICE, None, id(5), NEW, 0, b"x".to_vec(), T0)
            .await
            .unwrap();
        assert_eq!(
            two.send(BOB, None, id(5), EXISTING, 0, b"y".to_vec(), T0)
                .await,
            Err(Refusal::Gone)
        );
        assert_eq!(
            two.send(BOB, None, id(5), NEW, 0, b"x".to_vec(), T0).await,
            Err(Refusal::Gone),
            "another account cannot reopen a live pair id"
        );
        assert_eq!(
            two.receive(BOB, id(5), EXISTING, 0, T0).await.result,
            Err(Refusal::Gone)
        );
        assert_eq!(two.abort(BOB, id(5), T0).await, Ok(false));
        assert_eq!(two.abort(ALICE, id(5), T0).await, Ok(true));
        assert_eq!(
            one.receive(ALICE, id(5), EXISTING, 0, T0).await.result,
            Err(Refusal::Gone)
        );
        assert_eq!(one.abort(ALICE, id(5), T0).await, Ok(false));

        // Two pair ids drawing the same code are told apart.
        let mut led = [7u8; 16];
        let first = one.send(BOB, None, led, NEW, 0, b"x".to_vec(), T0).await;
        led[15] = 8;
        let second = two.send(BOB, None, led, NEW, 0, b"x".to_vec(), T0).await;
        assert_ne!(first.unwrap().code, second.unwrap().code);

        // Every session expires 300 s after it opened, on either handle.
        let later = T0 + SESSION_TTL_MS;
        let swept = two.receive(BOB, led, NEW, 0, later).await;
        assert_eq!(swept.result, Err(Refusal::Gone));
        assert!(swept.expired >= 1, "the sweep is reported");
        assert_eq!(
            one.receive(ALICE, id(3), EXISTING, 0, later).await.result,
            Err(Refusal::Gone)
        );

        // The pair-attempt limit is one budget across both handles: ten an
        // hour per account, and a refusal opens nothing.
        let t = T0 + 2 * SESSION_TTL_MS;
        for i in 0..10u8 {
            let node = if i % 2 == 0 { one } else { two };
            node.send("carol", Some("ip:a"), id(100 + i), NEW, 0, vec![i], t)
                .await
                .unwrap();
        }
        let refused = one
            .send("carol", Some("ip:b"), id(120), NEW, 0, vec![1], t + 1)
            .await;
        assert!(
            matches!(refused, Err(Refusal::Limited { retry_after_ms }) if retry_after_ms > 0),
            "{refused:?}"
        );
        assert_eq!(
            two.receive("carol", id(120), EXISTING, 0, t + 1)
                .await
                .result,
            Err(Refusal::Gone)
        );
        // Unlimited where no address is given, and code lookups keep a
        // budget of their own.
        one.send("carol", None, id(121), NEW, 0, vec![1], t + 2)
            .await
            .unwrap();
        assert_eq!(
            two.resolve("carol", Some("ip:a"), 999_999, t + 3)
                .await
                .result,
            Err(Refusal::Gone)
        );
        // The address budget, sixty an hour, spans accounts and handles.
        for i in 0..50u8 {
            let node = if i % 2 == 0 { two } else { one };
            node.resolve(&format!("dave{}", i / 10), Some("ip:c"), 1, t)
                .await;
        }
        for _ in 0..10 {
            one.resolve("erin", Some("ip:c"), 1, t).await;
        }
        assert!(matches!(
            two.resolve("frank", Some("ip:c"), 1, t + 1).await.result,
            Err(Refusal::Limited { .. })
        ));
    }
}
