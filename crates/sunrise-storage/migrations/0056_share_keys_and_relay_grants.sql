-- 0056: the two local halves of a share the op log does not carry (#1335,
-- docs/03-crypto/sharing-with-others.md §Granting access step 6, §Revoking
-- access step 3).
--
-- `held_share_keys` is the recipient's: one row per Stream-key epoch an
-- accepted share grant sealed to this account, keyed by the identity that
-- signed the grant and the *owner's* Stream id. It is not `stream_keys`, and
-- must never be folded into it: that table is this account's own key set,
-- every Stream id in it is rotated and its writable epochs published in the
-- key bundle, and its cache is keyed by `(stream_id, epoch)` alone. An owner
-- picks its Stream ids, so one that named a Stream id equal to one of this
-- account's own would put a key the owner knows among the keys this account
-- seals its own ops under (ADR-0074's "malicious owner"). Here the granter is
-- part of the key, and nothing that rotates, publishes or seals reads it.
--
-- `wrapped` is `stream_key_<epoch>` under this vault's root, as
-- `stream_keys.wrapped` is (`sunrise.wrap.stream_key.v1`, binding the Stream
-- id and epoch). `relay_grant_id` is the relay viewer grant the account
-- claimed for it, which a decline withdraws. `name` and `color` are what the
-- owner sealed beside the key; `expires_at_ms` is the grant's expiry, `NULL`
-- for never.
--
-- `share_relay_grants` is the owner's: the id of each relay viewer grant this
-- device registered for a share, so a revocation can withdraw it. The relay
-- names a grant only by that id, and no share op carries it, so it is this
-- device's alone: another of the account's devices that revokes the share
-- rotates the key, which is the guarantee, and has no grant to withdraw. A
-- row is deleted once the relay has let the grant go.
CREATE TABLE held_share_keys (
    granter_identity_id BLOB    NOT NULL,
    stream_id           BLOB    NOT NULL,
    epoch               INTEGER NOT NULL,
    wrapped             BLOB    NOT NULL,
    relay_grant_id      BLOB    NOT NULL,
    name                TEXT    NOT NULL,
    color               TEXT    NOT NULL,
    expires_at_ms       INTEGER,
    accepted_at_ms      INTEGER NOT NULL,
    PRIMARY KEY (granter_identity_id, stream_id, epoch)
) WITHOUT ROWID;

CREATE TABLE share_relay_grants (
    stream_id             BLOB    NOT NULL,
    recipient_identity_id BLOB    NOT NULL,
    relay_grant_id        BLOB    NOT NULL,
    created_at_ms         INTEGER NOT NULL,
    PRIMARY KEY (stream_id, recipient_identity_id, relay_grant_id)
) WITHOUT ROWID;
