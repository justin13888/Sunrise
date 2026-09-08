-- 0019: record which device minted the account identity, and clear
-- `identity.id_d_priv_wrapped` on every vault that provably did not.
--
-- Migration 0018 said in terms what it was not doing and why. A device paired
-- while STORAGE_V was 17 received `ID_D_priv` in its `PairingPayload` and
-- wrapped it into that column, so it goes on opening the identity-sealed copy
-- of every rotated epoch and revocation bounds nothing about it (issue #87).
-- Clearing the column unconditionally was not available: on the account's
-- *creator* it holds the only copy of `ID_D_priv` in existence -- no code path
-- writes a recovery blob yet -- and 0018's schema recorded nothing that
-- distinguished the two.
--
-- It recorded nothing *under that name*. `stream_keys.source` is a provenance
-- column 0017 added for a different purpose and it answers this question
-- exactly:
--
--   * The creator mints the vault-meta stream's first epoch itself, through
--     `Engine::ensure_base_epochs` -> `Keychain::mint_epoch`, which writes
--     source 'local'. A vault upgrading from pre-0017 mints the identity in
--     `Keychain::adopt_legacy_vault` and re-derives that same key with source
--     'legacy'; it is the creator too.
--   * A paired device never mints it. Epoch 1 of the vault-meta stream reaches
--     it in field 6 of the pairing payload ('pairing') or in a `key_envelope`
--     op ('envelope'), because the epoch already existed before the device did.
--
-- So "holds vault-meta epoch 1 with source 'local' or 'legacy'" is a positive,
-- durable proof of having minted the account identity, and its absence on a
-- vault that has an `identity` row at all is proof of the opposite.
--
-- The column makes the answer explicit rather than inferred from here on:
-- `Keychain::create` writes the minting device's id on the founder path and
-- leaves it NULL on the pairing path, and `Keychain::load` refuses to load
-- `ID_D_priv` unless the row names *this* device. Identity rotation will need
-- the same fact (issue #105), which is the other reason it is a column and not
-- a one-off `UPDATE`.
--
-- One case fails open and is named rather than papered over: a device paired
-- at 17 from a *brand-new* vault whose payload carried no stream keys at all
-- (the defect PR #86 fixed) minted vault-meta epoch 1 itself and is read here
-- as a creator. Such a vault is not a member of the account it paired from --
-- it holds a fork of the meta stream that no peer's key opens, so it can read
-- no control op and no rotated epoch -- and keeping `ID_D_priv` on it grants
-- access to nothing.
ALTER TABLE identity ADD COLUMN minted_by_device_id BLOB;

UPDATE identity
SET minted_by_device_id = (SELECT device_id FROM local_identity WHERE id = 1)
WHERE EXISTS (
    SELECT 1 FROM stream_keys
    WHERE stream_id = X'00000000000000000000000000000000'
      AND epoch = 1
      AND source IN ('local', 'legacy')
);

-- The revocation gap, closed: a paired device's copy of `ID_D_priv` goes, and
-- the creator's stays because the clause above named it.
UPDATE identity SET id_d_priv_wrapped = X'' WHERE minted_by_device_id IS NULL;
