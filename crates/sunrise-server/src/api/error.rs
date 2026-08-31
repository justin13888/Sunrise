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
//! [RFC 9457]: https://www.rfc-editor.org/rfc/rfc9457

use kynos::error::rejection::AuthRejection;

/// A failure this surface can return.
///
/// Every authentication and authorization failure collapses to
/// [`ApiError::Unauthenticated`], deliberately. The server distinguishes a bad
/// signature from an expired token from a revoked device in its own logs; a
/// client is told none of it, because the difference is only useful to someone
/// probing which accounts and devices exist.
#[derive(Debug, thiserror::Error, kynos::ApiError)]
#[problem(base = "https://sunrise.app/problems/")]
pub enum ApiError {
    /// The request was structurally valid but semantically wrong.
    #[error("{0}")]
    #[problem(status = 400, title = "Invalid request")]
    Validation(String),

    /// No credential, a bad one, or a device binding that did not check out.
    #[error("authentication required")]
    #[problem(status = 401, title = "Unauthenticated")]
    Unauthenticated,

    /// Authenticated, but not permitted to act on this resource.
    #[error("{0}")]
    #[problem(status = 403, title = "Forbidden")]
    Forbidden(String),

    /// No such resource on this account.
    #[error("{0}")]
    #[problem(status = 404, title = "Not found")]
    NotFound(String),

    /// Storage failed. Never carries the underlying message: a SQLite error
    /// string can name columns and constraints, which is the shape of an
    /// internal detail a client should not receive.
    #[error("internal error")]
    #[problem(status = 500, title = "Internal error")]
    Internal,
}

impl From<AuthRejection> for ApiError {
    fn from(_: AuthRejection) -> Self {
        Self::Unauthenticated
    }
}

impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        tracing::error!(
            ev = "srv.store.failed",
            err_kind = "internal",
            reason = %e,
            "storage error"
        );
        Self::Internal
    }
}
