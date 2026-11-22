-- 0055: the queue of "tell the relay this device is back" (ADR-0056 §5, #1299).
--
-- The inverse of 0020's `relay_revocation_intents`. Withdrawing a revocation
-- (`Command::WithdrawRevocation`) cancels an unsent telling in that table; once
-- the relay may already have been told, cancelling is not enough, and the
-- relay must be asked to accept the device again through
-- `POST /api/v1/devices/by-vault-id/{id}/reinstate`. That call needs a session
-- and the command must work offline, so it is a row here, written in the
-- command's own transaction, and the sync driver drains it.
--
-- One row per device. `attempts` and `last_attempt_ms` make a relay that keeps
-- refusing visible, as in 0020. `refused_at_ms` is the one column 0020 has no
-- counterpart for: the relay answers `403 DEVICE_NOT_REVOKER` when this device
-- is not the one it recorded as the revoker, which no retry changes and whose
-- remedy is pairing the device again. The row is kept with the time of that
-- answer, rather than deleted, so a client can still say so after the drain
-- has run. A row with it set is never sent again; a later withdrawal of a
-- fresh revocation of the same device writes the row anew.
CREATE TABLE relay_reinstatement_intents (
    device_id       BLOB PRIMARY KEY,
    created_at_ms   INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_ms INTEGER,
    refused_at_ms   INTEGER
) WITHOUT ROWID;
