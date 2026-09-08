-- 0020: the queue of "tell the relay this device is gone".
--
-- Revoking a device is two facts in two places. In the vault it is a
-- `device_revoke` op, sealed under the vault-meta Stream key, which the relay
-- holds no key for and so cannot read -- deliberately, because a relay that
-- could read it would learn which of an account's devices had been revoked and
-- when, for every account it serves. At the relay it is a `revoked` flag on its
-- own `devices` row, set through `DELETE /api/v1/devices/by-vault-id/{id}`.
--
-- Nothing connected the two (issue #80): the local command emitted the op and
-- the relay went on accepting the revoked device's uploads and streaming it
-- everyone else's. This table is the connection, and it is a table rather than
-- a direct call because `revoke_device` must work offline -- that is the
-- scenario, a device that is gone -- so the call cannot be part of the command.
--
-- One row per device, inserted in the same transaction as the op so the two
-- cannot diverge, and deleted only when the relay has answered. `attempts` and
-- `last_attempt_ms` exist so a relay that is refusing the call is visible
-- rather than silently retried forever.
CREATE TABLE relay_revocation_intents (
    device_id       BLOB PRIMARY KEY,
    created_at_ms   INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_ms INTEGER
) WITHOUT ROWID;
