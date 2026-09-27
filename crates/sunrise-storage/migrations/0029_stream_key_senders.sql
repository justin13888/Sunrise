-- 0029: which devices delivered each Stream key this vault absorbed, so a key a
-- read-bounded device holds is kept for reading and never written under.
--
-- The hole (issue #280)
-- ---------------------
--
-- `Keychain::absorb_stream_key` stored a key from a `key_envelope` and asked
-- nothing about who sealed it, and the live epoch a device writes under was
-- `MAX(epoch)` over `stream_keys`. So a device this replica had read-bounded --
-- a revoked device running a modified client, which is the threat revocation
-- exists for -- could mint a key at an epoch above the live one, seal it to
-- every honest device's public `D_D`, and read everything they wrote next.
-- `emit_key_envelopes`' anti-join kept it from being *given* a key; nothing
-- kept an honest device from *taking* one from it. The two local guards
-- (`revoke_device`'s `effective`, `rotate_stream_key`'s refusal) stop an
-- unmodified revoked client from emitting such a key, and bind nothing else.
--
-- Why a key is not simply refused
-- -------------------------------
--
-- Declining the key would leave this replica unable to open every op sealed
-- under it, including an honest peer's op written before that peer had
-- applied the revocation. Which ops a replica can read would then depend on
-- the order it met the envelope and the revocation, which is the divergence
-- ADR-0034 corollary 3 forbids. So the key is still absorbed. What changes is
-- which key this device *writes* under, and that was already a per-replica
-- choice.
--
-- What this table is
-- ------------------
--
-- One row per `(key, sender)` delivery of a `key_envelope` this device opened.
-- A delivery proves possession: the envelope's `key_id` is re-derived from the
-- opened key, so a device cannot deliver a key it does not hold. A key with a
-- sender in `device_read_bounds` is therefore one a read-bounded device
-- demonstrably holds, and `Keychain::current_epoch_tx` passes over it. The
-- test is evaluated when the live epoch is read, not when the key arrives, so a
-- revocation applied after the envelope still stops the key being written
-- under. A later delivery of the same key by a device the replica has not
-- bounded does not clear it: the first sender still holds the key.
--
-- Written only by `INSERT OR IGNORE` in `insert_stream_key_row`, and never
-- updated or deleted, like `stream_keys` itself.
--
-- The seed
-- --------
--
-- None. A key absorbed before this migration carries no record of who sealed
-- it, and nothing in the vault can reconstruct one: the `key_envelope` op in
-- `ops` is sealed, and its recipient and epoch are inside the ciphertext. Such
-- a key is never passed over. That residual is small: every rotation mints
-- above the highest held epoch, so a pre-upgrade key stops being live at the
-- stream's next rotation.
CREATE TABLE stream_key_senders (
    stream_id         BLOB NOT NULL,
    epoch             INTEGER NOT NULL,
    key_id            BLOB NOT NULL,
    sender_device_id  BLOB NOT NULL,
    PRIMARY KEY (stream_id, epoch, key_id, sender_device_id)
);
