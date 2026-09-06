-- 0018: an index of which device has been sent which `(stream, epoch)` key.
--
-- Before this, "has device D been sealed the key for (S, e)?" was unanswerable
-- from the database. The recipient lives *inside* the `key_envelope` op's
-- sealed inner payload, so the `ops` table cannot be queried for it: the column
-- that would carry it holds ciphertext.
--
-- Nothing needed the answer while every device held `ID_D_priv`, because a
-- device that missed its own copy opened the identity copy instead. Removing
-- `ID_D_priv` from `PairingPayload` (issue #76) removes that fallback, and
-- turns one previously harmless race into permanent unreadability: a device
-- that mints an epoch seals it to the devices *it* knows about, so a device
-- whose `device_cert` has not yet reached the minter is left out of that epoch
-- for good. `Engine::backfill_key_envelopes` closes it, and this table is what
-- lets a backfill emit only the envelopes that are actually missing rather than
-- re-sending every key it holds on every cert it sees.
--
-- Only device recipients are indexed. The identity copy is emitted for every
-- epoch unconditionally and is nobody's to be missing.
--
-- Deliberately empty on upgrade rather than reconstructed. Reconstructing it
-- would mean opening every `key_envelope` op in the log, which needs the Stream
-- keys and cannot run inside a migration. An empty index costs at most one
-- redundant round of envelopes the first time each device republishes its cert,
-- and absorbing a key already held is a no-op — so the failure mode of being
-- wrong here is bandwidth, never correctness.
-- What this migration deliberately does **not** do: clear
-- `identity.id_d_priv_wrapped`.
--
-- A device paired while STORAGE_V was 17 received `ID_D_priv` in its
-- `PairingPayload` and wrapped it into that column. Upgrading does not take it
-- away, so such a device keeps the identity's unwrapping key and revocation
-- does not bound its reads -- the exact state #76 describes. Clearing the
-- column unconditionally is not the fix: on the account's *creator* it is the
-- only copy of `ID_D_priv` in existence, no recovery blob is produced by any
-- code path yet, and the schema records nothing that distinguishes the two.
-- Blanking it would trade a revocation gap for a permanently unrecoverable
-- account.
--
-- STORAGE_V 17 was never released -- it merged the same day as this -- so the
-- exposure is development vaults, and re-pairing them clears it. Anything that
-- ships 17 to a user has to solve this properly first, by recording which
-- device minted the identity.
CREATE TABLE key_envelope_recipients (
    stream_id   BLOB    NOT NULL,
    epoch       INTEGER NOT NULL,
    -- The recipient device id, 16 bytes.
    recipient   BLOB    NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (stream_id, epoch, recipient)
) WITHOUT ROWID;
