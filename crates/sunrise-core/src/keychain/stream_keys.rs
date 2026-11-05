//! The Stream keys this device holds: the cache, the live epoch, and minting.

use crate::config::Rng;
use rusqlite::{params, OptionalExtension};
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::{unwrap_stream_key, StreamKey};
use sunrise_domain::PRIVATE_DOMAIN_BYTES;
use sunrise_storage::Db;
use zeroize::Zeroize;

#[cfg(test)]
use super::legacy::{legacy_derived_stream_key, LEGACY_EPOCH};
use super::rows::insert_stream_key_row;
use super::{
    keychain_row_error, to16, KeySource, Keychain, KeychainError, RotationSet, GENESIS_EPOCH,
    META_GENESIS_CONTEXT, WRITABLE_KEY,
};

impl Keychain {
    pub(super) fn load_cache(&self, db: &Db) -> Result<(), KeychainError> {
        let mut stmt = db
            .conn()
            .prepare("SELECT stream_id, epoch, wrapped FROM stream_keys")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut cache = self.cache.lock();
        for (stream_id, epoch, wrapped) in rows {
            // A row whose id is not 16 bytes is skipped for the same reason an
            // unopenable wrap is: it is one corrupt row, not a lost vault. It
            // is *not* zero-padded into `[0u8; 16]`, which is the vault-meta
            // stream and would file a stranger's key against it.
            let Some(stream_id) = to16(&stream_id) else {
                continue;
            };
            // A negative or out-of-`u32` epoch is skipped rather than clamped.
            // `unwrap_or(0)` filed the key at epoch 0 — a real slot on a real
            // stream — which is worse than dropping it: the row was not
            // skipped, it was *misfiled*, and a key filed at the wrong epoch
            // opens nothing while looking like a key that does.
            let Ok(epoch) = u32::try_from(epoch) else {
                continue;
            };
            // **A documented fail-open.** A row whose wrap does not open under
            // this root is skipped, not raised: the rest of the vault still
            // works, and refusing to open the whole keychain over one
            // unreadable row would turn a corrupt byte into a lost vault. The
            // cost is that this and "this device never received that key" are
            // the same outcome *in the cache* — which is why
            // `current_stream_key_tx` now tells them apart at the one point of
            // use where the difference decides something.
            if let Ok(key) = unwrap_stream_key(&self.vault_root, &wrapped, &stream_id, epoch) {
                cache.entry((stream_id, epoch)).or_default().push(key);
            }
        }
        Ok(())
    }

    /// **Every** `(stream_id, epoch)` this device holds a key for.
    ///
    /// It used to be `MAX(epoch)` per stream, and the reasoning recorded
    /// alongside it was that older epochs had already travelled in the new
    /// device's `PairingPayload`. That holds only when a device pairs and is
    /// certified in one uninterrupted step. A rotation between the two — and
    /// one is minted whenever a Stream is created, which is an ordinary event
    /// rather than a rare one — left the device with the payload's epochs and
    /// the current one, and a permanent hole where the epochs in between should
    /// have been. Ops sealed under those parked in `deferred_ops` until
    /// `engine::sync::DEFERRED_TTL_MS` dropped them, and nothing surfaced
    /// it: it presents as "some items from around when I set up this device
    /// never arrived". That was
    /// [#107](https://github.com/justin13888/Sunrise/issues/107).
    ///
    /// The cost of the wider set is real and is bounded twice over.
    /// `key_envelope_recipients` (migration 0018) records who has been sent
    /// which `(stream, epoch)`, so each pair is sealed to a given device **at
    /// most once per replica** however many certs it republishes — the `S x E x
    /// D` figure is the worst case of a first backfill, not a recurring one.
    /// And the epochs a replica holds are the epochs that stream has ever had
    /// on it, which grows only by rotation.
    ///
    /// A tempting third bound was rejected: deriving the set from the epochs
    /// still referenced by parked `deferred_ops`. Those rows live on the
    /// replica running the backfill and describe what *it* could not open — a
    /// replica that has been online throughout has none, and the device that
    /// actually needs the keys is the one whose state nobody here can see. It
    /// bounds the wrong replica's ignorance.
    ///
    /// Read from `stream_keys` rather than the in-memory cache so that a key
    /// absorbed inside the caller's own open transaction is included: the cache
    /// is loaded at open and refreshed on absorption, and a backfill running in
    /// the same transaction as the absorption that provoked it would otherwise
    /// miss exactly the epoch it was called about.
    ///
    /// Sorted so that two devices running the same backfill emit the same
    /// envelopes in the same order, which keeps a diff of two vaults' op logs
    /// readable.
    pub(crate) fn held_epochs_tx(
        tx: &rusqlite::Transaction<'_>,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT stream_id, epoch FROM stream_keys ORDER BY stream_id, epoch",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // **A documented fail-open.** A `stream_keys` row whose id is not 16
        // bytes, or whose epoch is outside `u32`, is dropped from the set this
        // returns — which is the set `backfill_key_envelopes` seals to a newly
        // certified device, so the device is never sent that `(stream, epoch)`
        // and its ops park until the deferred TTL drops them. That is #107's
        // failure mode arriving from a different direction.
        //
        // Left open, because the alternative is worse in the direction that
        // matters here. This runs on the *sender's* side of an admission, and a
        // hard error would refuse to backfill any of a newly paired device's
        // keys because one unrelated row in the table is malformed — turning a
        // corrupt byte on one stream into a device that can read nothing. The
        // rows that are skipped are rows this device cannot use either:
        // `current_epoch_tx` now refuses the same epoch and
        // `current_stream_key_tx` refuses the same wrap, so the stream is
        // already loudly broken locally and the peer's silence about it is not
        // what a reader will notice first.
        Ok(rows
            .into_iter()
            .filter_map(|(sid, epoch)| Some((to16(&sid)?, u32::try_from(epoch).ok()?)))
            .collect())
    }

    /// Every key this device holds at `(stream_id, epoch)`.
    ///
    /// Plural because two devices can mint the same epoch concurrently and both
    /// keys are retained; the AEAD tag decides which one actually opens an op.
    /// Losing one to a last-writer-wins on the minting op would lose every op
    /// sealed under it.
    ///
    /// # This is infallible on purpose, and what that costs
    ///
    /// An empty result is "no key here", and it does not distinguish "this
    /// device never received one" from "`load_cache` had a row and could not
    /// open it". That is a **documented fail-open**: the caller is the deferral
    /// path, which parks an op it cannot open and retries it after every
    /// absorbed key, and parking is the right answer for the common case by a
    /// very large margin — a `key_envelope` that has not arrived yet is
    /// ordinary and self-resolving.
    ///
    /// The case where the difference decides something is the *live* key at the
    /// *current* epoch, because there the answer decides whether to mint. That
    /// is [`Self::current_stream_key_tx`], which reads the row rather than this
    /// cache and now raises rather than reporting an absence.
    #[must_use]
    pub fn stream_keys_at(&self, stream_id: &[u8; 16], epoch: u32) -> Vec<StreamKey> {
        let cached: Vec<StreamKey> = self
            .cache
            .lock()
            .get(&(*stream_id, epoch))
            .cloned()
            .unwrap_or_default();
        #[cfg(test)]
        if self.test_derived_keys && cached.is_empty() {
            return vec![legacy_derived_stream_key(
                &self.vault_root,
                stream_id,
                epoch,
            )];
        }
        cached
    }

    /// The live epoch for `stream_id`: the one this device seals its own writes
    /// under, read from the table inside `tx`.
    ///
    /// On an ordinary stream it is the highest epoch held, **provided that
    /// epoch holds a key no read-bounded device delivered** (issue #280,
    /// ADR-0041 §Decision 4). A key a read-bounded device sealed to this one
    /// is still stored, still in the cache, and still opens every op sealed
    /// under it; it is only never chosen to write under. Without that, a
    /// revoked device running a modified client could mint an epoch above the
    /// live one, seal it to every honest device's public `D_D`, and read what
    /// they wrote next, because this was `MAX(epoch)`.
    ///
    /// When the highest epoch is not writable the answer is `None`, never an
    /// older writable epoch below it. The epoch above exists because a
    /// rotation superseded the ones below, and a device an earlier revocation
    /// cut held every epoch sealed before its cut. Falling back would write
    /// under one of them: with Q revoked and R the sole sender of the
    /// rotation that cut Q, bounding R later must not send this device back
    /// to an epoch Q holds.
    ///
    /// What this cannot see is who holds a key minted here or delivered by a
    /// current device. An epoch live before a revocation stays live after it,
    /// on a replica where no bounded device delivered it, and the revoked
    /// device holds it until the rotation's envelope arrives and moves the
    /// highest epoch past it.
    ///
    /// The bound is read here, at the moment of choosing, and not when the
    /// key arrived, so a revocation applied after the envelope still takes
    /// effect. It is `device_read_bounds`, the engine's per-replica ratchet,
    /// for the reason every key-distribution site reads it: the class to keep
    /// out is the devices whose reads are bounded, and an unwound device is
    /// in it while the register has stopped naming it.
    ///
    /// # When no key is writable
    ///
    /// That happens on a stream whose highest epoch the revoked device
    /// delivered here, by its own rotation or by `backfill_key_envelopes` when
    /// this device's cert arrived. An epoch minted here, or delivered only by
    /// current devices, stays writable. What happens next depends on the
    /// stream.
    ///
    /// * **An ordinary stream reads as having no live key**, so
    ///   `Engine::ensure_stream_epoch` mints one above every held key and
    ///   seals it to every unbounded device. On the revoking device the
    ///   rotation has already done that; elsewhere it is a mint the rotation's
    ///   own envelope would otherwise have supplied a moment later.
    /// * **The vault-meta stream never mints here.** It reads as its highest
    ///   writable epoch and, failing one, as the highest epoch held whoever
    ///   delivered it. It carries the revocation itself, and the revocation
    ///   is sealed under the epoch the cut device still holds so that the cut
    ///   device, and every peer that has not yet received the rotation, can
    ///   read it. A meta mint here would also seal its own `key_envelope`s
    ///   under the epoch being minted, which no other device can open. The
    ///   residual is a window: on a replica that has bounded a device and not
    ///   yet received the rotation's meta key, meta ops are written under an
    ///   epoch a revoked device holds, either one the bounded device planted
    ///   or an older one an earlier-revoked device was sealed before its cut.
    ///   The rotation's envelopes follow the revocation in the same stream,
    ///   so the window closes as the rotation is applied.
    ///
    /// [`Self::max_epoch_tx`] is the other question, "what is the highest
    /// epoch this device holds at all", and it is what minting counts from.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn current_epoch_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        if *stream_id == crate::engine::META_STREAM {
            let sql = format!(
                "SELECT MAX(k.epoch) FROM stream_keys k WHERE k.stream_id = ? AND {WRITABLE_KEY}"
            );
            return match Self::epoch_query(tx, &sql, stream_id)? {
                None => self.max_epoch_tx(tx, stream_id),
                found => Ok(found),
            };
        }
        // The highest held epoch, and only if it is writable. Never an older
        // one below it: the epoch above exists because someone rotated, and
        // the epochs it superseded are held by whoever that rotation cut.
        let Some(max) = self.max_epoch_tx(tx, stream_id)? else {
            return Ok(None);
        };
        let writable: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM stream_keys k
                 WHERE k.stream_id = ? AND k.epoch = ? AND {WRITABLE_KEY})"
            ),
            params![&stream_id[..], max],
            |r| r.get(0),
        )?;
        Ok(writable.then_some(max))
    }

    /// The highest epoch this device holds a key for on `stream_id`, whoever
    /// delivered it.
    ///
    /// Minting counts from this and not from [`Self::current_epoch_tx`], so a
    /// fresh epoch lands above every key a read-bounded device delivered
    /// rather than beside one of them. The `engine::sync::MAX_EPOCH_LEAP` bound on an
    /// absorbed epoch reads it too: it is a bound on how far this device's
    /// epochs can be dragged, which is a question about every key it holds.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn max_epoch_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        Self::epoch_query(
            tx,
            "SELECT MAX(epoch) FROM stream_keys WHERE stream_id = ?",
            stream_id,
        )
    }

    fn epoch_query(
        tx: &rusqlite::Transaction<'_>,
        sql: &str,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        let epoch: Option<i64> = tx
            .query_row(sql, params![&stream_id[..]], |r| r.get(0))
            .optional()?
            .flatten();
        // A negative or out-of-`u32` epoch used to read as `1`, the genesis
        // epoch — a valid-looking answer that sends the key lookup to a row
        // that is not there and `mint_epoch` to overwrite a stream that is far
        // past it. There is no safe substitute for an epoch, so there is no
        // substitute.
        epoch
            .map(|e| u32::try_from(e).map_err(|_| keychain_row_error("stream_keys.epoch")))
            .transpose()
    }

    /// The live `(epoch, key)` for `stream_id`, or `None` if this device holds
    /// no key for it at all.
    ///
    /// When two devices minted the same epoch this returns the lowest `key_id`
    /// among the keys [`Self::current_epoch_tx`] may choose, so a device's
    /// choice of which to seal under is deterministic rather than dependent on
    /// row order, and a key a read-bounded device delivered at the live epoch
    /// is passed over there too.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn current_stream_key_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<(u32, StreamKey)>> {
        // A derived-key test keychain always "has" every key, so an engine unit
        // test never mints one and never emits the `key_envelope` ops it has no
        // relay to carry. Without this the first vault-meta op of every test
        // would mint that stream's key mid-transaction and emit envelope ops
        // into the very stream whose sequence number the caller had already
        // read.
        #[cfg(test)]
        if self.test_derived_keys {
            return Ok(Some((
                LEGACY_EPOCH,
                legacy_derived_stream_key(&self.vault_root, stream_id, LEGACY_EPOCH),
            )));
        }
        let Some(epoch) = self.current_epoch_tx(tx, stream_id)? else {
            return Ok(None);
        };
        // A writable key first, then the lowest `key_id`. At an epoch
        // `current_epoch_tx` chose for holding a writable key, that is the
        // lowest writable one. At the vault-meta fallback epoch none is
        // writable, and it is the plain lowest.
        let wrapped: Option<Vec<u8>> = tx
            .query_row(
                &format!(
                    "SELECT k.wrapped FROM stream_keys k
                     WHERE k.stream_id = ? AND k.epoch = ?
                     ORDER BY CASE WHEN {WRITABLE_KEY} THEN 0 ELSE 1 END, k.key_id
                     LIMIT 1"
                ),
                params![&stream_id[..], epoch],
                |r| r.get(0),
            )
            .optional()?;
        let Some(wrapped) = wrapped else {
            return Ok(None);
        };
        // An unopenable *live* key is an error, not an absence.
        //
        // `Ok(None)` here meant "this stream has never had a key", which is
        // what line 1 of this function returns when the epoch row is genuinely
        // missing — and the sole caller, `Engine::ensure_stream_epoch`, answers
        // it by minting. `mint_epoch` reads `MAX(epoch)` and bumps it, so the
        // mint is not a repair: it is epoch N+1, broadcast to every peer as
        // `key_envelope` ops, with every subsequent op sealed under it. One
        // corrupt or wrong-root row therefore produced a full, silent,
        // unannounced rotation — no revocation, no operator action, no error —
        // and parked every peer's ops at epoch N until the deferred TTL dropped
        // them. The corrupt row survived and never re-triggered, because
        // `MAX(epoch)` now pointed past it.
        //
        // `load_cache` still skips such a row at open (one unreadable row is
        // not a lost vault, and that is the right call there, because the rest
        // of the vault works). This is the point of use, where the answer
        // decides whether to mint, and here the two cases have to be told
        // apart.
        match unwrap_stream_key(&self.vault_root, &wrapped, stream_id, epoch) {
            Ok(key) => Ok(Some((epoch, key))),
            Err(_) => Err(keychain_row_error("stream_keys.wrapped")),
        }
    }

    /// Mint the next epoch for `stream_id`: 32 fresh random bytes, wrapped and
    /// persisted, returned so the caller can seal `key_envelope` ops with it.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn mint_epoch(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        rng: &dyn Rng,
        now_ms: u64,
    ) -> rusqlite::Result<(u32, StreamKey)> {
        // Above every key held, not only above the live one. Minting beside a
        // key a read-bounded device delivered would share its epoch number,
        // and on a replica that has not bounded that device the two would tie
        // on epoch and be split by `key_id`, a coin the minter does not hold.
        // It also keeps the one derived key, the vault-meta genesis, from ever
        // being minted over a row that already holds it: epoch 1 is chosen
        // only when this stream holds no key at all.
        let epoch = self
            .max_epoch_tx(tx, stream_id)?
            .map_or(1, |e| e.saturating_add(1));
        let key = self.fresh_key(stream_id, epoch, rng);
        insert_stream_key_row(
            tx,
            &self.vault_root,
            stream_id,
            epoch,
            &key,
            KeySource::Local,
            rng,
            now_ms,
        )?;
        self.cache_insert(stream_id, epoch, &key);
        Ok((epoch, key))
    }

    /// Record a Stream key this device learned from somewhere other than its
    /// own minting. Idempotent on `(stream_id, epoch, key_id)`.
    ///
    /// Returns `true` if the key was new to this device, which is what tells
    /// the caller to drain [`deferred_ops`](crate::engine) for that key.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn absorb_stream_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        source: KeySource,
        rng: &dyn Rng,
        now_ms: u64,
    ) -> rusqlite::Result<bool> {
        let changed = insert_stream_key_row(
            tx,
            &self.vault_root,
            stream_id,
            epoch,
            key,
            source,
            rng,
            now_ms,
        )?;
        if changed {
            self.cache_insert(stream_id, epoch, key);
        }
        Ok(changed)
    }

    /// 32 fresh random bytes — the whole point of ADR-0024, and the reason a
    /// Stream key can be rotated at all.
    // `self` is read only by the `cfg(test)` branch below, which is the whole
    // point of it being a method: a derived-key test keychain has to mint the
    // key the other replica would derive. Taking `&self` unconditionally keeps
    // one call site rather than two.
    #[allow(clippy::unused_self)]
    fn fresh_key(&self, stream_id: &[u8; 16], epoch: u32, rng: &dyn Rng) -> StreamKey {
        #[cfg(test)]
        if self.test_derived_keys {
            return legacy_derived_stream_key(&self.vault_root, stream_id, epoch);
        }
        if let Some(key) = self.meta_genesis_key(stream_id, epoch) {
            return key;
        }
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        let key = StreamKey::from_bytes(bytes);
        bytes.zeroize();
        key
    }

    /// The vault-meta stream's **genesis** key, derived from the account
    /// identity instead of drawn at random. `None` for every other
    /// `(stream, epoch)` and on any device that does not hold `ID_D_priv`.
    ///
    /// # Why one key in this hierarchy is derived
    ///
    /// ADR-0024 decision 4 seals every `(stream_id, epoch)` key to the
    /// identity's `ID_D_pub` as well as to each device, "so the recovery path
    /// can reach them". Every one of those `key_envelope` ops lives in the
    /// **vault-meta stream**, and an op in a stream is sealed under that
    /// stream's key — including, at epoch 1, the ops that carry that very key.
    /// [`crate::engine::Engine::emit_key_envelopes`] passes `seal_under: None`
    /// at a genesis mint, and `emit_control_op` resolves that to the
    /// vault-meta epoch it is in the middle of minting.
    ///
    /// So the identity copy of the vault-meta genesis key is sealed under the
    /// vault-meta genesis key. A recovering device holds `ID_D_priv` and can
    /// open nothing with it: the outer AEAD never yields the inner HPKE
    /// ciphertext, the op parks in `deferred_ops`, and every later epoch —
    /// which is sealed under its predecessor, correctly — is stranded behind
    /// it. The vault syncs, applies nothing, and presents as an empty account,
    /// which is what `docs/03-crypto/recovery.md` §Recovery flow step 8 warns
    /// looks exactly like success.
    ///
    /// Deriving this one key from `ID_D_priv` breaks the cycle and nothing
    /// else: the recovering vault recomputes the founder's vault-meta genesis
    /// key, opens the log, and takes every other Stream key out of it through
    /// the identity envelopes that were already being emitted.
    ///
    /// # What it costs
    ///
    /// Nothing that `ID_D_priv` did not already cost. `recovery.md` §The cost
    /// of that, stated plainly already records that `ID_D_priv` is a long-lived
    /// unwrapping key whose compromise reaches every epoch ever sealed to it;
    /// this makes the genesis epoch reachable by the same key rather than by a
    /// second one. It is gated by the same 256-bit recovery code through the
    /// same Argon2id, it adds no primitive outside ADR-0004's frozen suite
    /// (BLAKE3 `derive_key`, which already produces `device_id` and
    /// `stream_key_id`), and it changes no stored or wire format: the result is
    /// an ordinary `stream_keys` row.
    ///
    /// A device admitted by **pairing** holds no `ID_D_priv` and returns `None`
    /// here, which is correct and costs it nothing: its payload carried the
    /// account's epochs, so it never mints a vault-meta genesis at all.
    pub(super) fn meta_genesis_key(&self, stream_id: &[u8; 16], epoch: u32) -> Option<StreamKey> {
        if *stream_id != crate::engine::META_STREAM || epoch != GENESIS_EPOCH {
            return None;
        }
        // Under the lock for the same reason every other reader is: identity
        // rotation swaps this in place, and a genesis key derived from half of
        // one identity and half of another would open nothing.
        let state = self.identity.lock();
        let dh = state.identity.dh_secret.as_ref()?;
        let mut ikm = [0u8; 48];
        let mut secret = dh.secret_bytes();
        ikm[..32].copy_from_slice(&secret);
        ikm[32..].copy_from_slice(&state.identity.identity_id);
        secret.zeroize();
        let mut bytes = derive_key_32(META_GENESIS_CONTEXT, &ikm);
        ikm.zeroize();
        let key = StreamKey::from_bytes(bytes);
        bytes.zeroize();
        Some(key)
    }

    fn cache_insert(&self, stream_id: &[u8; 16], epoch: u32, key: &StreamKey) {
        let mut cache = self.cache.lock();
        let slot = cache.entry((*stream_id, epoch)).or_default();
        if !slot.iter().any(|k| k == key) {
            slot.push(key.clone());
        }
    }

    /// Every `(stream_id, live epoch)` this device holds a key for, **and the
    /// rows it could not turn into one**.
    ///
    /// This is the rotation set: revoking a device mints a new epoch for every
    /// stream in it, the vault-meta stream and the private domain included
    /// (ADR-0046 §2: it rotates with every Stream, without exception).
    ///
    /// # Why this returns two lists and not one
    ///
    /// A `stream_keys`, `streams` or `ops` row whose `stream_id` is not 16
    /// bytes cannot be rotated — the id names nothing, and there is no stream
    /// to mint an epoch for. It is *not* zero-padded into `[0u8; 16]`, which is
    /// the vault-meta stream and would file a stranger's key against it.
    ///
    /// Dropping it silently is the part that was wrong. This is the revocation
    /// set, so a row dropped here is a stream the revoked device goes on
    /// reading — and `revoke_device` used to return success having not rotated
    /// it. A security operation that reports success without having done the
    /// thing is the failure this module spent the rest of its audit removing.
    ///
    /// Raising instead would be worse, and that is not the alternative. The
    /// user's answer to "my laptop was stolen" must not be an error that leaves
    /// *every* stream unrotated, because the device that is gone is the whole
    /// scenario. So the revocation succeeds partially and says so: the caller
    /// rotates [`RotationSet::streams`] and reports
    /// [`RotationSet::unrotatable`], and a user learns their revocation is
    /// incomplete and which rows it could not reach.
    ///
    /// The same distinction one level up is issue #160 and
    /// [`Core::relay_revocation_pending`](crate::Core::relay_revocation_pending):
    /// a revocation has halves with different guarantees and a user is entitled
    /// to know which they have.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn rotation_set(&self, tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<RotationSet> {
        let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        let mut unrotatable: std::collections::BTreeSet<Vec<u8>> =
            std::collections::BTreeSet::new();
        streams.insert(crate::engine::META_STREAM);
        streams.insert(PRIVATE_DOMAIN_BYTES);
        for sql in [
            "SELECT DISTINCT stream_id FROM stream_keys",
            "SELECT stream_id FROM streams",
            "SELECT DISTINCT stream_id FROM ops",
        ] {
            let mut stmt = tx.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                let raw = row?;
                // A `BTreeSet`, because the same malformed blob can appear in
                // all three queries and a user counting "streams I could not
                // rotate" must not be told three.
                match to16(&raw) {
                    Some(id) => {
                        streams.insert(id);
                    }
                    None => {
                        unrotatable.insert(raw);
                    }
                }
            }
        }
        Ok(RotationSet {
            streams: streams.into_iter().collect(),
            unrotatable: unrotatable.into_iter().collect(),
        })
    }
}
