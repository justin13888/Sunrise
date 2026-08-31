//! The one error shape this surface returns.
//!
//! kynos renders every failure as an [RFC 9457] problem document, including its
//! own extractor rejections — a body that will not parse, a header that will
//! not decode — and each appears in the operation's `responses` because
//! `FromRequestParts::Rejection` is required to describe itself. Declaring the
//! handler's own failures the same way is what keeps the description's
//! `responses` complete rather than "whatever the handler happened to build".
//!
//! `status` is read once and becomes the `statuses()` const, the short-circuit
//! const and the `Responses` keys, so those three cannot disagree.
//!
//! # Why the `code` survives the envelope change
//!
//! The envelope moved from `{"error":{"code","message"}}` to a problem
//! document, but [`codes`] is a *client* contract — `docs/06-server/api.md`
//! §errors says clients map these to translated strings — and a client cannot
//! map a `type` URI it has never seen. Each variant therefore carries its code
//! as an RFC 9457 **extension member**, published beside `type`, `title`,
//! `status` and `detail` rather than in place of them. The problem document is
//! the envelope; the code is still the discriminator.
//!
//! [RFC 9457]: https://www.rfc-editor.org/rfc/rfc9457

use kynos::error::rejection::AuthRejection;

/// Stable error codes.
///
/// A client contract: clients map these to translated strings, so they are
/// constants rather than strings formatted at the call site.
pub mod codes {
    /// Token signature, issuer, or audience did not validate. Also the code
    /// every device-binding failure collapses to — see [`super::ApiError`].
    pub const AUTH_TOKEN_INVALID: &str = "AUTH_TOKEN_INVALID";
    /// First login for an unknown account while `allow_signup = false`.
    pub const AUTH_SIGNUP_DISABLED: &str = "AUTH_SIGNUP_DISABLED";
    /// Caller is not an active device of the account.
    pub const AUTH_DEVICE_NOT_OWNER: &str = "AUTH_DEVICE_NOT_OWNER";
    /// The named device is not on this account.
    pub const DEVICE_NOT_FOUND: &str = "DEVICE_NOT_FOUND";
    /// Malformed body or field.
    pub const VALIDATION_INVALID: &str = "VALIDATION_INVALID";
    /// A chunk's ciphertext BLAKE3 disagrees with the hash the client supplied.
    pub const BLOB_HASH_MISMATCH: &str = "BLOB_HASH_MISMATCH";
    /// `finalize` names a chunk that was never uploaded.
    pub const BLOB_CHUNK_MISSING: &str = "BLOB_CHUNK_MISSING";
    /// No committed blob under that id for this account.
    pub const BLOB_NOT_FOUND: &str = "BLOB_NOT_FOUND";
    /// Server-side failure.
    pub const FATAL_INTERNAL: &str = "FATAL_INTERNAL";
}

/// A failure this surface can return.
///
/// Every authentication and device-binding failure collapses to
/// [`ApiError::Unauthenticated`] carrying [`codes::AUTH_TOKEN_INVALID`],
/// deliberately. The server distinguishes a bad signature from an expired token
/// from a revoked device in its own logs; a client is told none of it, because
/// the difference is only useful to someone probing which accounts and devices
/// exist.
///
/// The one distinction a client *does* need — "your token expired, refresh and
/// retry" — travels as a `WWW-Authenticate` challenge, which is what RFC 6750
/// defines for exactly that and which does not require a second code.
#[derive(Debug, thiserror::Error, kynos::ApiError)]
#[problem(base = "https://sunrise.app/problems/")]
pub enum ApiError {
    /// The request was structurally valid but semantically wrong.
    #[error("{message}")]
    #[problem(status = 400, title = "Invalid request")]
    Validation {
        /// The stable client-facing code.
        #[problem(extension)]
        code: &'static str,
        /// Safe-for-logs summary: never quotes a token, a key, or a subject.
        message: String,
    },

    /// No credential, a bad one, or a device binding that did not check out.
    #[error("authentication required")]
    #[problem(status = 401, title = "Unauthenticated")]
    Unauthenticated {
        /// Always [`codes::AUTH_TOKEN_INVALID`]; see the type's own docs for
        /// why this carries no finer distinction.
        #[problem(extension)]
        code: &'static str,
    },

    /// First login for an unknown account while `allow_signup = false`.
    ///
    /// Separate from [`ApiError::Forbidden`] because it is a statement about
    /// the *server's* policy rather than about the caller, and a client shows
    /// it as "this server is not accepting new accounts" rather than as a
    /// permission failure.
    #[error("sign-up is disabled on this server")]
    #[problem(status = 403, title = "Sign-up disabled")]
    SignupDisabled {
        /// Always [`codes::AUTH_SIGNUP_DISABLED`].
        #[problem(extension)]
        code: &'static str,
    },

    /// Authenticated, but not permitted to act on this resource.
    #[error("{message}")]
    #[problem(status = 403, title = "Forbidden")]
    Forbidden {
        /// The stable client-facing code.
        #[problem(extension)]
        code: &'static str,
        /// Safe-for-logs summary.
        message: String,
    },

    /// No such resource on this account.
    #[error("{message}")]
    #[problem(status = 404, title = "Not found")]
    NotFound {
        /// The stable client-facing code.
        #[problem(extension)]
        code: &'static str,
        /// Safe-for-logs summary.
        message: String,
    },

    /// The request is well-formed but the resource is not in a state that
    /// admits it — a `finalize` naming a chunk that never arrived.
    #[error("{message}")]
    #[problem(status = 409, title = "Conflict")]
    Conflict {
        /// The stable client-facing code.
        #[problem(extension)]
        code: &'static str,
        /// Safe-for-logs summary.
        message: String,
    },

    /// Storage failed. Never carries the underlying message: a SQLite error
    /// string can name columns and constraints, which is the shape of an
    /// internal detail a client should not receive.
    #[error("internal error")]
    #[problem(status = 500, title = "Internal error")]
    Internal {
        /// Always [`codes::FATAL_INTERNAL`].
        #[problem(extension)]
        code: &'static str,
    },
}

impl ApiError {
    /// `400 VALIDATION_INVALID`.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            code: codes::VALIDATION_INVALID,
            message: message.into(),
        }
    }

    /// `400` with a caller-chosen code, for the blob failures that are
    /// separately actionable.
    #[must_use]
    pub fn validation_coded(code: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            code,
            message: message.into(),
        }
    }

    /// `401 AUTH_TOKEN_INVALID`. The only way to build a 401 on this surface,
    /// which is what makes the collapse a property rather than a convention.
    #[must_use]
    pub fn unauthenticated() -> Self {
        Self::Unauthenticated {
            code: codes::AUTH_TOKEN_INVALID,
        }
    }

    /// `403 AUTH_SIGNUP_DISABLED`.
    #[must_use]
    pub fn signup_disabled() -> Self {
        Self::SignupDisabled {
            code: codes::AUTH_SIGNUP_DISABLED,
        }
    }

    /// `403` with a stable code.
    #[must_use]
    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::Forbidden {
            code,
            message: message.into(),
        }
    }

    /// `404` with a stable code.
    #[must_use]
    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::NotFound {
            code,
            message: message.into(),
        }
    }

    /// `409` with a stable code.
    #[must_use]
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    /// `500 FATAL_INTERNAL`.
    #[must_use]
    pub fn internal() -> Self {
        Self::Internal {
            code: codes::FATAL_INTERNAL,
        }
    }
}

impl From<AuthRejection> for ApiError {
    /// Both of kynos's rejections land on the collapse.
    ///
    /// `AuthRejection` carries a status and an optional challenge and nothing
    /// application-shaped, so there is no finer distinction available here even
    /// if one were wanted — and the type's own docs say one is not.
    fn from(_: AuthRejection) -> Self {
        Self::unauthenticated()
    }
}

impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        match e {
            // A server-policy refusal, not a caller failure: 403 with its own
            // code, matching what the surface this replaces returned.
            crate::store::StoreError::SignupDisabled => Self::signup_disabled(),
            crate::store::StoreError::NotFound => Self::not_found(
                codes::DEVICE_NOT_FOUND,
                "no such record on this account".to_owned(),
            ),
            crate::store::StoreError::Sqlite(_) => {
                tracing::error!(
                    ev = "srv.store.failed",
                    err_kind = "internal",
                    reason = %e,
                    "storage error"
                );
                Self::internal()
            }
        }
    }
}
