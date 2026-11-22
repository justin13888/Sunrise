//! The pairing rendezvous in two shared tables, so either half of a pairing
//! can reach any relay node and the pair-attempt limits are one budget across
//! the cluster (#1262).
//!
//! The policy is `api::pairing`'s, unchanged: every decision goes through its
//! `Session`, `free_code` and `admit_both`, so the single-binary relay and a
//! cluster answer alike. What is here is only where the state lives.
//!
//! - **No pair id in a key.** A session row is keyed by `BLAKE3(pair_id)`, as
//!   a sync session's is by the hash of its id. The pair id itself is held
//!   only while the session's typed code is live, because `resolve` hands it
//!   back for the code; it is cleared with the code when the existing device
//!   joins. A ledger row is keyed by the hash of its account or address, so
//!   the table lists no client address either.
//! - **A send is one transaction**, the session row read `FOR UPDATE`, so two
//!   nodes buffering into one session take turns rather than lose a message.
//! - **Opens of one account take turns** under an advisory lock on the
//!   account, so two nodes never give two live sessions of it one code.
//! - **An attempt ledger is a row** holding its admitted timestamps, upserted
//!   and so locked by the charge that reads it, the account's before the
//!   address's: two charges of one key take turns and never deadlock.
//! - **Expiry.** Every call first deletes the sessions it finds expired and
//!   reports how many, as the in-memory rendezvous does; and every charge
//!   deletes ledgers idle for a day. Both skip rows another call holds, so a
//!   sweep never waits on, or deadlocks with, a send.
//! - **A store that does not answer** is [`Refusal::Unavailable`], logged as
//!   `srv.store.failed`, and nothing is half-written: the transaction rolls
//!   back.

use std::collections::HashSet;

use sunrise_pairing::AttemptWindow;
use tokio_postgres::{GenericClient, Row, Transaction};

use super::{key16, signed, unsigned, PgStore};
use crate::api::pairing::{
    admit_both, free_code, PairRole, Received, Refusal, Rendezvous, Resolved, Sent, Session,
    MAX_SESSIONS,
};

/// The first key of every account's two-key pairing lock: "SRPA". Distinct
/// from the relay log's channel and account classes.
const PAIR_LOCK: i32 = 0x5352_5041;

/// How long a ledger row outlives the last attempt it holds: the longest
/// window an attempt limit looks back over.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Idle ledgers one charge deletes at most, so the sweep stays one short
/// statement however many a quiet day left behind.
const SWEEP_BATCH: i64 = 1000;

/// One of the four attempt ledgers, as its `pairing_attempts.ledger` value.
#[derive(Debug, Clone, Copy)]
#[repr(i16)]
enum Ledger {
    OpenByAccount = 0,
    OpenByAddress = 1,
    LookupByAccount = 2,
    LookupByAddress = 3,
}

/// The BLAKE3 of `parts`, each length-prefixed so no two lists collide.
fn hashed(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    for part in parts {
        h.update(&u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(part);
    }
    h.finalize().as_bytes().to_vec()
}

/// The second key of `account_id`'s pairing lock.
fn lock_key(account_id: &str) -> i32 {
    let h = hashed(&[account_id.as_bytes()]);
    i32::from_le_bytes([h[0], h[1], h[2], h[3]])
}

/// What a store failure answers: unavailable, with the cause logged and never
/// sent.
fn failed(e: impl std::fmt::Display) -> Refusal {
    tracing::error!(
        ev = "srv.store.failed",
        err_code = %sunrise_error::ErrorCode::RelayStorageUnavailable,
        err_kind = "transient",
        retryable = true,
        cause = %e,
        "the shared pairing rendezvous did not answer"
    );
    Refusal::Unavailable
}

const SELECT_SESSION: &str = "SELECT account_id, expires_at_ms, code, new_device, existing
     FROM pairing_sessions WHERE pair_h = $1";

fn session_of(row: &Row) -> Session {
    let code: Option<i32> = row.get(2);
    Session {
        account_id: row.get(0),
        expires_at_ms: unsigned(row.get(1)),
        code: code.and_then(|c| u32::try_from(c).ok()),
        sent: [row.get(3), row.get(4)],
    }
}

/// The Postgres [`Rendezvous`], on the same pool as the metadata.
#[derive(Debug, Clone)]
pub struct PgRendezvous {
    store: PgStore,
}

impl PgRendezvous {
    /// Pairing sessions and ledgers in `store`'s database.
    #[must_use]
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, Refusal> {
        self.store.client().await.map_err(failed)
    }
}

/// Delete the expired sessions no other call holds; how many went.
async fn reap(c: &impl GenericClient, now_ms: u64) -> Result<usize, Refusal> {
    let n = c
        .execute(
            "DELETE FROM pairing_sessions WHERE pair_h IN (
                 SELECT pair_h FROM pairing_sessions WHERE expires_at_ms <= $1
                 FOR UPDATE SKIP LOCKED)",
            &[&signed(now_ms)],
        )
        .await
        .map_err(failed)?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/// Charge one attempt at `now_ms` to `account_id` in `by_account` and to
/// `address` in `by_address`, or refuse it with the longer of the two waits,
/// charging neither. Runs in `tx`, whose commit is the charge.
async fn charge(
    tx: &Transaction<'_>,
    (by_account, account_id): (Ledger, &str),
    (by_address, address): (Ledger, &str),
    now_ms: u64,
) -> Result<(), Refusal> {
    // The account's row first, then the address's: every charge locks in
    // this order, so two never wait on each other.
    let keys = [
        (by_account as i16, hashed(&[account_id.as_bytes()])),
        (by_address as i16, hashed(&[address.as_bytes()])),
    ];
    let mut windows = Vec::with_capacity(2);
    for (ledger, key) in &keys {
        let held: Vec<i64> = tx
            .query_one(
                "INSERT INTO pairing_attempts AS a (ledger, key_h, admitted_ms, idle_at_ms)
                 VALUES ($1, $2, '{}', 0)
                 ON CONFLICT (ledger, key_h) DO UPDATE SET ledger = a.ledger
                 RETURNING admitted_ms",
                &[ledger, key],
            )
            .await
            .map_err(failed)?
            .get(0);
        windows.push(AttemptWindow::from_admitted(held.into_iter().map(unsigned)));
    }
    let [mut account, mut client]: [AttemptWindow; 2] =
        windows.try_into().map_err(|_| Refusal::Unavailable)?;
    admit_both(&mut account, &mut client, now_ms)?;
    for ((ledger, key), window) in keys.iter().zip([account, client]) {
        let admitted: Vec<i64> = window.admitted().map(signed).collect();
        let idle_at = admitted.last().copied().unwrap_or(0) + signed(DAY_MS);
        tx.execute(
            "UPDATE pairing_attempts SET admitted_ms = $3, idle_at_ms = $4
             WHERE ledger = $1 AND key_h = $2",
            &[ledger, key, &admitted, &idle_at],
        )
        .await
        .map_err(failed)?;
    }
    Ok(())
}

/// Delete ledgers with no attempt in the last day that no other call holds.
async fn sweep_ledgers(c: &impl GenericClient, now_ms: u64) -> Result<(), Refusal> {
    c.execute(
        "DELETE FROM pairing_attempts WHERE (ledger, key_h) IN (
             SELECT ledger, key_h FROM pairing_attempts WHERE idle_at_ms <= $1
             LIMIT $2 FOR UPDATE SKIP LOCKED)",
        &[&signed(now_ms), &SWEEP_BATCH],
    )
    .await
    .map_err(failed)?;
    Ok(())
}

/// The session under `key`, read `FOR UPDATE`.
async fn locked(tx: &Transaction<'_>, key: &[u8]) -> Result<Option<Session>, Refusal> {
    let row = tx
        .query_opt(&format!("{SELECT_SESSION} FOR UPDATE"), &[&key])
        .await
        .map_err(failed)?;
    Ok(row.as_ref().map(session_of))
}

/// Open a session of `account_id` under `key` in `tx`: the session table's
/// bound, then the attempt limits, then a code no live session of the account
/// holds. `Ok(None)` when another account opened the same pair id first.
async fn open(
    tx: &Transaction<'_>,
    key: &[u8],
    account_id: &str,
    address: Option<&str>,
    pair_id: [u8; 16],
    now_ms: u64,
) -> Result<Option<Session>, Refusal> {
    let row = tx
        .query_one(
            "SELECT COUNT(*), MIN(expires_at_ms) FROM pairing_sessions WHERE expires_at_ms > $1",
            &[&signed(now_ms)],
        )
        .await
        .map_err(failed)?;
    let live: i64 = row.get(0);
    if usize::try_from(live).unwrap_or(usize::MAX) >= MAX_SESSIONS {
        let soonest: Option<i64> = row.get(1);
        return Err(Refusal::Limited {
            retry_after_ms: soonest
                .map_or(1, |at| unsigned(at).saturating_sub(now_ms))
                .max(1),
        });
    }
    if let Some(address) = address {
        charge(
            tx,
            (Ledger::OpenByAccount, account_id),
            (Ledger::OpenByAddress, address),
            now_ms,
        )
        .await?;
    }
    let taken: HashSet<u32> = tx
        .query(
            "SELECT code FROM pairing_sessions WHERE account_id = $1 AND code IS NOT NULL",
            &[&account_id],
        )
        .await
        .map_err(failed)?
        .iter()
        .filter_map(|r| u32::try_from(r.get::<_, i32>(0)).ok())
        .collect();
    let code = free_code(&taken, pair_id);
    let session = Session::open(account_id, code, now_ms);
    let inserted = tx
        .execute(
            "INSERT INTO pairing_sessions (pair_h, account_id, expires_at_ms, code, pair_id)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (pair_h) DO NOTHING",
            &[
                &key,
                &account_id,
                &signed(session.expires_at_ms),
                &i32::try_from(code).map_err(failed)?,
                &&pair_id[..],
            ],
        )
        .await
        .map_err(failed)?;
    Ok((inserted == 1).then_some(session))
}

#[async_trait::async_trait]
impl Rendezvous for PgRendezvous {
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
        let mut c = self.client().await?;
        let mut expired = reap(&**c, now_ms).await?;
        if address.is_some() && role == PairRole::NewDevice && index == 0 {
            sweep_ledgers(&**c, now_ms).await?;
        }
        let key = hashed(&[&pair_id[..]]);
        let tx = c.transaction().await.map_err(failed)?;
        // One a sweep skipped because another call held it.
        expired += usize::try_from(
            tx.execute(
                "DELETE FROM pairing_sessions WHERE pair_h = $1 AND expires_at_ms <= $2",
                &[&key, &signed(now_ms)],
            )
            .await
            .map_err(failed)?,
        )
        .unwrap_or(0);
        let mut held = locked(&tx, &key).await?;
        let mut opened = false;
        if held.is_none() {
            // Only the new device's first message opens a session, as in
            // memory.
            if role != PairRole::NewDevice || index != 0 {
                return Err(Refusal::Gone);
            }
            tx.execute(
                "SELECT pg_advisory_xact_lock($1, $2)",
                &[&PAIR_LOCK, &lock_key(account_id)],
            )
            .await
            .map_err(failed)?;
            // Another node of this account may have opened it while this one
            // waited for the lock.
            held = locked(&tx, &key).await?;
            if held.is_none() {
                held = match open(&tx, &key, account_id, address, pair_id, now_ms).await? {
                    Some(session) => {
                        opened = true;
                        Some(session)
                    }
                    None => locked(&tx, &key).await?,
                };
            }
        }
        let Some(mut session) = held.filter(|s| s.account_id == account_id) else {
            return Err(Refusal::Gone);
        };
        let Some(replayed) = session.take(role, index, message) else {
            tx.execute("DELETE FROM pairing_sessions WHERE pair_h = $1", &[&key])
                .await
                .map_err(failed)?;
            tx.commit().await.map_err(failed)?;
            return Err(Refusal::Gone);
        };
        if !replayed {
            let code = session.code.and_then(|c| i32::try_from(c).ok());
            tx.execute(
                "UPDATE pairing_sessions
                 SET code = $2, pair_id = CASE WHEN $2::INTEGER IS NULL THEN NULL ELSE pair_id END,
                     new_device = $3, existing = $4
                 WHERE pair_h = $1",
                &[
                    &key,
                    &code,
                    &session.sent_by(PairRole::NewDevice),
                    &session.sent_by(PairRole::ExistingDevice),
                ],
            )
            .await
            .map_err(failed)?;
        }
        tx.commit().await.map_err(failed)?;
        Ok(session.answer(role, opened, replayed, expired))
    }

    async fn resolve(
        &self,
        account_id: &str,
        address: Option<&str>,
        code: u32,
        now_ms: u64,
    ) -> Resolved {
        let mut expired = 0;
        let result = async {
            let mut c = self.client().await?;
            expired = reap(&**c, now_ms).await?;
            if let Some(address) = address {
                sweep_ledgers(&**c, now_ms).await?;
                let tx = c.transaction().await.map_err(failed)?;
                charge(
                    &tx,
                    (Ledger::LookupByAccount, account_id),
                    (Ledger::LookupByAddress, address),
                    now_ms,
                )
                .await?;
                tx.commit().await.map_err(failed)?;
            }
            let row = c
                .query_opt(
                    "SELECT pair_id, expires_at_ms FROM pairing_sessions
                     WHERE account_id = $1 AND code = $2 AND expires_at_ms > $3",
                    &[
                        &account_id,
                        &i32::try_from(code).map_err(failed)?,
                        &signed(now_ms),
                    ],
                )
                .await
                .map_err(failed)?
                .ok_or(Refusal::Gone)?;
            let pair_id: Option<Vec<u8>> = row.get(0);
            let pair_id = pair_id.as_deref().and_then(key16).ok_or(Refusal::Gone)?;
            Ok((pair_id, unsigned(row.get(1))))
        }
        .await;
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
        let mut expired = 0;
        let result = async {
            let c = self.client().await?;
            expired = reap(&**c, now_ms).await?;
            let row = c
                .query_opt(SELECT_SESSION, &[&hashed(&[&pair_id[..]])])
                .await
                .map_err(failed)?;
            row.as_ref()
                .map(session_of)
                .filter(|s| s.account_id == account_id && s.expires_at_ms > now_ms)
                .map(|s| (s.for_peer_of(role, after), s.expires_at_ms))
                .ok_or(Refusal::Gone)
        }
        .await;
        Received { result, expired }
    }

    async fn abort(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        now_ms: u64,
    ) -> Result<bool, Refusal> {
        let c = self.client().await?;
        reap(&**c, now_ms).await?;
        let n = c
            .execute(
                "DELETE FROM pairing_sessions
                 WHERE pair_h = $1 AND account_id = $2 AND expires_at_ms > $3",
                &[&hashed(&[&pair_id[..]]), &account_id, &signed(now_ms)],
            )
            .await
            .map_err(failed)?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{isolated, url};
    use super::{hashed, PgRendezvous, PgStore};
    use crate::api::pairing::{PairRole, Refusal, Rendezvous as _};

    const T0: u64 = 1_800_000_000_000;

    /// Two nodes' rendezvous over one fresh database, or `None` without one.
    async fn two_nodes() -> Option<(PgRendezvous, PgRendezvous, PgStore)> {
        let url = url()?;
        let connect = isolated(&url).await;
        let a = PgStore::open(connect.clone()).await.unwrap();
        let b = PgStore::open(connect).await.unwrap();
        Some((PgRendezvous::new(a.clone()), PgRendezvous::new(b), a))
    }

    /// The contract every rendezvous owes, each step on alternating nodes.
    #[tokio::test]
    async fn the_postgres_rendezvous_passes_the_rendezvous_conformance_suite() {
        let Some((one, two, _)) = two_nodes().await else {
            return;
        };
        crate::api::pairing::rendezvous_conformance(&one, &two).await;
    }

    /// Twenty opens of one account at once, across two nodes, spend one
    /// budget of ten, and the sessions they open hold ten distinct codes.
    #[tokio::test]
    async fn concurrent_opens_across_nodes_share_one_attempt_budget() {
        let Some((one, two, _)) = two_nodes().await else {
            return;
        };
        let mut opens = Vec::new();
        for i in 0..20u8 {
            let node = if i % 2 == 0 { one.clone() } else { two.clone() };
            // Every pair id draws the same code, so each open must step past
            // the others'.
            let mut pair_id = [i; 16];
            pair_id[..4].copy_from_slice(&[0, 0, 0, 1]);
            opens.push(tokio::spawn(async move {
                node.send(
                    "acct",
                    Some("ip:a"),
                    pair_id,
                    PairRole::NewDevice,
                    0,
                    vec![1],
                    T0,
                )
                .await
            }));
        }
        let mut codes = Vec::new();
        let mut refused = 0;
        for open in opens {
            match open.await.unwrap() {
                Ok(sent) => codes.push(sent.code.unwrap()),
                Err(Refusal::Limited { .. }) => refused += 1,
                Err(other) => panic!("{other:?}"),
            }
        }
        assert_eq!((codes.len(), refused), (10, 10));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 10, "no two live sessions share a code");
    }

    /// A session row is keyed by the pair id's hash, and holds the pair id
    /// itself only until the existing device joins; a ledger row holds no
    /// address.
    #[tokio::test]
    async fn a_session_row_holds_no_pair_id_once_joined() {
        let Some((one, two, store)) = two_nodes().await else {
            return;
        };
        let pair_id = [0x42; 16];
        one.send(
            "acct",
            Some("ip:203.0.113.9"),
            pair_id,
            PairRole::NewDevice,
            0,
            vec![1],
            T0,
        )
        .await
        .unwrap();
        let c = store.client().await.unwrap();
        let row = c
            .query_one("SELECT pair_h, pair_id FROM pairing_sessions", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, Vec<u8>>(0), hashed(&[&pair_id[..]]));
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(1),
            Some(pair_id.to_vec()),
            "held for the code lookup"
        );
        two.send(
            "acct",
            None,
            pair_id,
            PairRole::ExistingDevice,
            0,
            vec![2],
            T0 + 1,
        )
        .await
        .unwrap();
        let row = c
            .query_one("SELECT pair_id, code FROM pairing_sessions", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, Option<Vec<u8>>>(0), None);
        assert_eq!(row.get::<_, Option<i32>>(1), None);
        let keys = c
            .query("SELECT key_h FROM pairing_attempts", &[])
            .await
            .unwrap();
        assert_eq!(keys.len(), 2);
        for key in keys {
            let key: Vec<u8> = key.get(0);
            assert!(!key.windows(9).any(|w| w == b"203.0.113"), "{key:?}");
        }
    }
}
