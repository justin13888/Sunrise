//! The known contents.
//!
//! Every value below is written by the generator and asserted by the tests, so
//! "unchanged" means "still equal to the constant the fixture was built from"
//! rather than "equal to whatever we read a moment ago".

/// The pre-0017 Inbox: sixteen zero bytes, which was also the vault-meta
/// stream id.
pub(super) const LEGACY_INBOX: [u8; 16] = [0u8; 16];
/// Where 0017 moves it.
pub(super) const REMAPPED_INBOX: [u8; 16] = *b"\x00\x00\x00sunrise.inbox";

pub(super) const STREAM_ALPHA: [u8; 16] = [0xa1; 16];
pub(super) const STREAM_BETA: [u8; 16] = [0xb2; 16];
pub(super) const STREAM_GAMMA: [u8; 16] = [0xc3; 16];

pub(super) const TASK_IN_INBOX: [u8; 16] = [0x01; 16];
pub(super) const TASK_IN_ALPHA: [u8; 16] = [0x02; 16];
pub(super) const INBOX_TASK_TITLE: &str = "Renew the passport";
pub(super) const ALPHA_TASK_TITLE: &str = "Ship the migration fixture";
/// A `NoteBody`, opaque to SQL and to this test — which is the point: it is
/// checked byte for byte.
pub(super) const ALPHA_TASK_BODY: &[u8] = b"\x82\x01\x6fblock-grammar-v1";

pub(super) const CONTEXT_ERRANDS: [u8; 16] = [0xd4; 16];

pub(super) const DEVICE_LAPTOP: [u8; 16] = [0xf6; 16];
pub(super) const DEVICE_RETIRED: [u8; 16] = [0xf7; 16];
pub(super) const RETIRED_AT_MS: i64 = 1_700_000_000_000;

pub(super) const OP_ID: [u8; 16] = [0xe5; 16];
/// A sealed envelope. Nothing in the storage layer may rewrite these bytes —
/// the signature covers them — so this is the strictest content assertion in
/// the fixtures.
pub(super) const OP_ENVELOPE: &[u8] = b"\xa4\x01\x58\x20sealed-op-envelope-not-rewritable";

pub(super) const SIGNING_SECRET_WRAPPED: &[u8] = b"\x11\x22\x33wrapped-device-signing-seed";
pub(super) const DEVICE_CERT_BLOB: &[u8] = b"\xa3\x01\x50self-issued-device-cert";

// --- v17-only contents ----------------------------------------------------

/// The vault-meta stream: sixteen zero bytes, and never a `streams` row.
pub(super) const VAULT_META_STREAM: [u8; 16] = [0u8; 16];
pub(super) const IDENTITY_ID: [u8; 16] = [0x21; 16];
pub(super) const ID_S_PUB: [u8; 32] = [0x22; 32];
pub(super) const ID_D_PUB: [u8; 32] = [0x23; 32];
pub(super) const ID_S_PRIV_WRAPPED: &[u8] = b"\x24wrapped-ID_S_priv";
/// The one the 0019 migration decides the fate of.
pub(super) const ID_D_PRIV_WRAPPED: &[u8] = b"\x25wrapped-ID_D_priv";
pub(super) const DH_SECRET_WRAPPED: &[u8] = b"\x26wrapped-device-X25519-secret";
pub(super) const META_KEY_ID: &[u8] = b"\x01\x02\x03\x04\x05\x06\x07\x08";
pub(super) const ALPHA_KEY_ID: &[u8] = b"\x08\x07\x06\x05\x04\x03\x02\x01";
pub(super) const WRAPPED_STREAM_KEY: &[u8] = b"\x27wrapped-stream-key";
