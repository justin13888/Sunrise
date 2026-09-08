//! Account creation + recovery flow.
//!
//! Per `docs/03-crypto/recovery.md` and `docs/06-server/api.md` §account.
//! v1 surface is the data shapes the client sends to / receives from the
//! server. Transport-layer plumbing lives in the per-platform clients.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_errors_doc)]

pub mod account;
pub mod recovery;

pub use account::{
    decode_recovery_blob, encode_public_key, encode_recovery_blob, AccountCreateRequest,
    AccountInfo,
};
pub use recovery::{recover_identity, recover_identity_from_code, RecoveryFlowError};
