//! The REST error envelope from `docs/06-server/api.md` §errors.
//!
//! ```json
//! { "error": { "code": "AUTH_SIGNUP_DISABLED", "message": "…" } }
//! ```
//!
//! Codes are a stable client contract — clients map them to translated
//! strings — so they are constants here rather than formatted at the call
//! site. Messages are safe-for-logs: they never quote a token, a key, or a
//! subject.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth::AuthError;
use crate::store::StoreError;

/// Stable error codes.
pub mod codes {
    /// Token signature, issuer, or audience did not validate.
    pub const AUTH_TOKEN_INVALID: &str = "AUTH_TOKEN_INVALID";
    /// Token `exp` is in the past.
    pub const AUTH_TOKEN_EXPIRED: &str = "AUTH_TOKEN_EXPIRED";
    /// First login for an unknown account while `allow_signup = false`.
    pub const AUTH_SIGNUP_DISABLED: &str = "AUTH_SIGNUP_DISABLED";
    /// Caller is not an active device of the account.
    pub const AUTH_DEVICE_NOT_OWNER: &str = "AUTH_DEVICE_NOT_OWNER";
    /// The named device is not on this account.
    pub const DEVICE_NOT_FOUND: &str = "DEVICE_NOT_FOUND";
    /// Malformed body or field.
    pub const VALIDATION_INVALID: &str = "VALIDATION_INVALID";
    /// Server-side failure.
    pub const FATAL_INTERNAL: &str = "FATAL_INTERNAL";
}

/// One REST error.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP status to return.
    pub status: StatusCode,
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Human-readable, safe-for-logs message.
    pub message: String,
}

#[derive(Debug, Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Debug, Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
}

impl ApiError {
    /// Construct.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// `400 VALIDATION_INVALID`.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, codes::VALIDATION_INVALID, message)
    }

    /// `401 AUTH_TOKEN_INVALID`.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, codes::AUTH_TOKEN_INVALID, message)
    }

    /// `500 FATAL_INTERNAL`. The underlying error is deliberately not
    /// reflected to the caller.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            codes::FATAL_INTERNAL,
            "internal error",
        )
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Expired => Self::new(
                StatusCode::UNAUTHORIZED,
                codes::AUTH_TOKEN_EXPIRED,
                "access token has expired",
            ),
            // Transport failures are the *server's* problem, but answering 503
            // would tell an unauthenticated caller that its token got as far as
            // a JWKS fetch. Both map to the same opaque 401.
            AuthError::Missing | AuthError::Invalid(_) | AuthError::Transport(_) => {
                Self::unauthorized("access token is missing or not valid for this server")
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::SignupDisabled => Self::new(
                StatusCode::FORBIDDEN,
                codes::AUTH_SIGNUP_DISABLED,
                "sign-up is disabled on this server",
            ),
            StoreError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                codes::DEVICE_NOT_FOUND,
                "no such record on this account",
            ),
            StoreError::Sqlite(_) => Self::internal(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(Body {
                error: Inner {
                    code: self.code,
                    message: &self.message,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn renders_the_documented_envelope() {
        let res = ApiError::validation("email required").into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "VALIDATION_INVALID");
        assert_eq!(v["error"]["message"], "email required");
    }

    /// An expired token is separately actionable — the client refreshes and
    /// retries — so it must not collapse into the generic invalid code.
    #[tokio::test]
    async fn expiry_is_distinguishable_from_invalidity() {
        let expired: ApiError = AuthError::Expired.into();
        assert_eq!(expired.code, codes::AUTH_TOKEN_EXPIRED);
        let invalid: ApiError = AuthError::Invalid("bad sig".into()).into();
        assert_eq!(invalid.code, codes::AUTH_TOKEN_INVALID);
    }

    /// A JWKS outage must not be reported to an unauthenticated caller as
    /// anything other than "your token did not work".
    #[test]
    fn transport_failures_do_not_leak_server_state() {
        let e: ApiError = AuthError::Transport("dns".into()).into();
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
        assert!(!e.message.contains("dns"));
    }

    #[test]
    fn a_sqlite_failure_never_reaches_the_client_verbatim() {
        let e: ApiError = StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows).into();
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.message, "internal error");
    }
}
