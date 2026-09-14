-- 0023: the three values the identity-chain fold has to read and 0022 did not
-- give it.
--
-- 0022 built the chain: `identity_transitions` and the `genesis_identity_id`
-- anchor to fold it from. Writing the fold found it could not be *verified*
-- from those columns alone, for two reasons that are both about what a
-- verifier already has to hold rather than about what the op carries.

-- --- the genesis identity's public signing key ---
-- The fold walks genesis -> head and needs `(identity_id, ID_S_pub)` per link:
-- the id to match the next transition's `from_identity_id`, and the key to
-- check that transition's `prev_sig`, which is the outgoing identity
-- authorizing the hand-over.
--
-- Every successor's key is in its own row's `to_id_s_pub`. The genesis's is in
-- no row at all: `identity.id_s_pub` holds the identity *in force*, and
-- `Keychain::adopt_successor_identity` overwrites it on the first rotation. The
-- value is not recoverable afterwards -- `identity_id` is
-- `BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub)[..16]`, which is one
-- way -- so a vault that rotated once could never again verify a cert issued
-- before it did, and every device admitted before the rotation would stop
-- being verifiable rather than merely stop being current.
--
-- Carrying the predecessor's key in the op instead was the alternative and is
-- worse: it would be a field of the signed body that the verifier has no
-- independent way to check, which is the same mistake as trusting
-- `DeviceCert.identity_id` without recomputing it.
ALTER TABLE identity ADD COLUMN genesis_id_s_pub BLOB;

-- Exact on every vault: nothing has rotated, so the identity in force is the
-- genesis. The same argument 0022 makes for `genesis_identity_id`.
UPDATE identity SET genesis_id_s_pub = id_s_pub;

-- --- the two digests the signatures are taken over ---
-- `prev_sig` and `next_sig` sign
-- `BLAKE3(canonical_cbor({from, to, to_id_s_pub, to_id_d_pub, roster_digest,
-- shares_digest}))`, so a verifier needs all six. Four are columns; the two
-- digests were inside the `payload` blob only.
--
-- Reading them out of the blob would have meant decoding the whole
-- `IdentityTransitionPayload` -- roster, shares and all -- on every link of
-- every fold, and the fold runs at open. Worse, it would put a CBOR decoder in
-- the path that decides which identity the account has, where the columns
-- beside it are already the denormalization the table exists for. They are
-- columns for the same reason `hlc_physical_ms` is one: the fold must not have
-- to open anything to run.
--
-- `X''` as the default rather than NULL so the column is NOT NULL and a link
-- that somehow arrived without one fails closed: an empty digest is not a
-- BLAKE3 output, so no signature verifies over it. No existing row can be
-- affected -- `identity_transitions` was created empty one migration ago and
-- nothing can have written to it, because nothing can emit a transition yet.
ALTER TABLE identity_transitions ADD COLUMN roster_digest BLOB NOT NULL DEFAULT X'';
ALTER TABLE identity_transitions ADD COLUMN shares_digest BLOB NOT NULL DEFAULT X'';
