//! Account creation + recovery flow.
//!
//! Per `docs/03-crypto/recovery.md` and `docs/06-server/api.md` §account.
//! The surface is the data shapes the client sends to / receives from the
//! server, and the recovery flow every client runs
//! ([`recovery::recover_account`]). The relay is injected through
//! [`recovery::RecoveryRelay`]; `sunrise-relay-client` implements it, so
//! transport plumbing stays out of this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_errors_doc)]

pub mod account;
pub mod recovery;

pub use account::{
    decode_public_key, decode_recovery_blob, encode_public_key, encode_recovery_blob,
    AccountCreateRequest, AccountInfo, PublicKeyError,
};
pub use recovery::{
    check_recovery_code, identity_id_from_account, is_recovery_word, recover_account,
    recover_identity, recover_identity_from_code, rejoin_account, restore_identity, DeviceLabel,
    Recovered, RecoveringDevice, RecoveryFlowError, RecoveryProgress, RecoveryRelay, RelayRefusal,
    RestoreError, SyncStarter,
};
