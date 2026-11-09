//! The rendezvous's state: live sessions, their typed codes, and the
//! pair-attempt and code-lookup ledgers, behind the routes in `pairing.rs`.
//!
//! Apart from the handlers because it changes for a different reason: this is
//! the policy (who may open, read and look up what, and how often), driven by
//! a test clock, and the handlers are its HTTP shape.

use super::{PairRole, MESSAGES_PER_ROLE, SESSION_TTL_MS};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use sunrise_pairing::{AttemptLimit, AttemptWindow};

/// Live sessions the relay holds at once, across every account. Past it a new
/// session is refused as rate-limited until one expires, rather than letting
/// memory grow with whoever opens the most.
pub(super) const MAX_SESSIONS: usize = 4096;

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
}

impl Refusal {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::Limited { .. } => "rate_limited",
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

/// The in-memory rendezvous: live sessions and the pair-attempt ledgers.
///
/// Memory, not the store: a session lives five minutes and holds ciphertext
/// nobody can use after it, so a restart losing every one costs the users in
/// the middle of a pairing one rescan, and persisting it would put pairing
/// traffic on disk for no reader.
///
/// Every method takes `now_ms`, so the policy is driven by a test clock rather
/// than by sleeping.
#[derive(Debug, Clone, Default)]
pub struct Rendezvous {
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

#[derive(Debug)]
struct Session {
    account_id: String,
    expires_at_ms: u64,
    /// The typed code, until the existing device sends its first message.
    code: Option<u32>,
    /// Indexed by [`PairRole::index`]: what each side has sent.
    sent: [Vec<Vec<u8>>; 2],
}

impl Rendezvous {
    /// An empty rendezvous.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

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
    /// the session table is full.
    #[allow(clippy::too_many_arguments)]
    pub fn send(
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
            let code = inner.free_code(account_id, pair_id);
            inner.sessions.insert(
                pair_id,
                Session {
                    account_id: account_id.to_owned(),
                    expires_at_ms: now_ms + SESSION_TTL_MS,
                    code: Some(code),
                    sent: [Vec::new(), Vec::new()],
                },
            );
        }
        let Some(session) = inner
            .sessions
            .get_mut(&pair_id)
            .filter(|s| s.account_id == account_id)
        else {
            return Err(Refusal::Gone);
        };
        let mine = &mut session.sent[role.index()];
        let replayed = mine.get(index).map(|held| *held == message);
        match replayed {
            // A retry of a send whose answer was lost: acknowledged, not
            // buffered twice.
            Some(true) => {}
            // The index is taken by a different message, or is past the next
            // free one, or past the protocol's three: the two sides no longer
            // agree on where the exchange is, and nothing either sends now
            // could decrypt. The spec drops the session on overflow; the same
            // answer serves the rest.
            Some(false) => {
                inner.sessions.remove(&pair_id);
                return Err(Refusal::Gone);
            }
            None if index != mine.len() || index >= MESSAGES_PER_ROLE => {
                inner.sessions.remove(&pair_id);
                return Err(Refusal::Gone);
            }
            None => mine.push(message),
        }
        let count = u32::try_from(mine.len()).unwrap_or(u32::MAX);
        if role == PairRole::ExistingDevice {
            // The existing device has joined: the code has done its work, and
            // a second lookup must not hand the session to anyone else.
            session.code = None;
        }
        Ok(Sent {
            count,
            opened,
            replayed: replayed == Some(true),
            expires_at_ms: session.expires_at_ms,
            expired,
            code: session.code.filter(|_| role == PairRole::NewDevice),
        })
    }

    /// The session `account_id` holds under `code`, charging one lookup to
    /// `address` and the account where `address` is given.
    ///
    /// # Errors
    ///
    /// [`Refusal::Limited`] when the lookup is over either limit, and
    /// [`Refusal::Gone`] when no live session of this account holds `code`.
    pub fn resolve(
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

    /// What `role`'s peer has sent, from index `after` on.
    #[must_use]
    pub fn receive(
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
            .map(|s| {
                let theirs = &s.sent[role.peer().index()];
                let fresh = theirs.get(after..).unwrap_or_default().to_vec();
                (fresh, s.expires_at_ms)
            })
            .ok_or(Refusal::Gone);
        Received { result, expired }
    }

    /// Drop the session, if this account holds it. Whether one was dropped.
    pub fn abort(&self, account_id: &str, pair_id: [u8; 16], now_ms: u64) -> bool {
        let mut inner = self.inner.lock();
        inner.reap(now_ms);
        let ours = inner
            .sessions
            .get(&pair_id)
            .is_some_and(|s| s.account_id == account_id);
        if ours {
            inner.sessions.remove(&pair_id);
        }
        ours
    }

    /// Sessions currently live, expired ones included until the next call.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().sessions.len()
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

    /// A code for a new session of `account_id`: drawn from the random
    /// `pair_id`, then stepped past every code a live session of the same
    /// account holds. The table holds far fewer than [`CODE_SPACE`] sessions,
    /// so the step ends.
    ///
    /// The taken codes are collected once rather than rescanned per step: the
    /// `pair_id` is the client's to choose, so a client can make every one of
    /// its sessions draw the same code, and a rescan per step would make each
    /// open cost the square of the table.
    fn free_code(&self, account_id: &str, pair_id: [u8; 16]) -> u32 {
        let taken: HashSet<u32> = self
            .sessions
            .values()
            .filter(|s| s.account_id == account_id)
            .filter_map(|s| s.code)
            .collect();
        let [a, b, c, d, ..] = pair_id;
        let mut code = u32::from_be_bytes([a, b, c, d]) % CODE_SPACE;
        while taken.contains(&code) {
            code = (code + 1) % CODE_SPACE;
        }
        code
    }
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
    let waits = [
        account.try_admit(AttemptLimit::PER_ACCOUNT, now_ms).err(),
        client.try_admit(AttemptLimit::PER_ADDRESS, now_ms).err(),
    ];
    if let Some(retry_after_ms) = waits.into_iter().flatten().max() {
        return Err(Refusal::Limited { retry_after_ms });
    }
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
