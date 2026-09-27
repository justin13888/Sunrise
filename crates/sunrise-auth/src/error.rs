//! Login failures.

use thiserror::Error;

/// Anything that can go wrong obtaining or renewing a bearer.
///
/// Deliberately coarse where a finer grain would be a hint. `Rejected` covers
/// every way the issuer said no, because distinguishing "no such user" from
/// "wrong password" from "MFA failed" is the issuer's business and is
/// user-account enumeration if it is ours.
#[derive(Debug, Error)]
pub enum LoginError {
    /// The issuer URL, discovery document, or an endpoint it names is not
    /// usable — wrong scheme, wrong issuer, missing endpoint.
    #[error("issuer configuration: {0}")]
    Provider(String),
    /// Network, TLS, or HTTP failure talking to the issuer.
    #[error("issuer transport: {0}")]
    Transport(String),
    /// The issuer's response did not parse.
    #[error("issuer response: {0}")]
    Malformed(String),
    /// The issuer declined the request: `error=` in the callback, or a token
    /// response carrying one of RFC 6749 §5.2's error codes.
    ///
    /// Only an answer the issuer gave in OAuth's own words. A token endpoint
    /// that could not be reached is [`LoginError::Transport`], and one whose
    /// failure did not parse, or named a code outside §5.2, is
    /// [`LoginError::Malformed`] or [`LoginError::Transport`]: a client drops
    /// a refresh token on this variant, and must not on a network blink or an
    /// outage page.
    #[error("issuer declined: {0}")]
    Rejected(String),
    /// The redirect came back with the wrong `state`.
    ///
    /// Its own variant because it is not a transport hiccup: a mismatched
    /// `state` means the browser delivered a response to an authorization
    /// request this process did not make. Treat it as hostile.
    #[error("redirect state did not match the request; discarding")]
    StateMismatch,
    /// Could not bind, accept on, or read from the loopback redirect socket.
    #[error("redirect listener: {0}")]
    Redirect(String),
    /// The user did not finish in the browser before the deadline.
    #[error("timed out waiting for the browser redirect")]
    TimedOut,
    /// The browser could not be launched.
    #[error("could not open a browser: {0}")]
    Browser(String),
}
