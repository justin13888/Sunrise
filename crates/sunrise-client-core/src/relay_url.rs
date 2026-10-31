//! What counts as a relay URL: one rule for every client.
//!
//! Before this module there were three. The Apple app's sync plan took any
//! string that started `http://` or `https://`, path and all; the pairing seam
//! took `https://` to anywhere and `http://` to a loopback host, path and all;
//! and the web client took an **origin** — `https://`, `http://` on loopback
//! only, nothing after the host and port. A relay URL a user typed could so
//! connect sync and then be refused for pairing, or work on the Mac and not in
//! the browser.
//!
//! The rule now is the web's, which is the strictest of the three and the one
//! every consumer already needs:
//!
//! * **`https://`, or `http://` to a loopback host.** The URL is where a client
//!   sends the account bearer — sync on every request, pairing on every poll
//!   — and a bearer over plain HTTP to another machine is one anybody on the
//!   path can replay. Loopback (`localhost`, `127.0.0.0/8`, `::1`) is a relay
//!   on this machine, which is how development and the tests run one.
//! * **An origin and nothing else.** The relay client takes a base origin and
//!   appends `/api/v1/…` itself (`sunrise-relay-client`), and the pairing QR
//!   names an origin (`docs/03-crypto/pairing-and-onboarding.md`). A path, a
//!   query, a fragment or credentials is refused rather than dropped:
//!   silently discarding part of what a user typed would connect them
//!   somewhere they did not say. A trailing `/` is not a path.
//! * **At most [`MAX_RELAY_URL_LEN`] bytes**, the pairing QR's `relay_url`
//!   bound, so any relay a device is configured for fits in a code. Both what
//!   was typed and the origin it serialises to, which IDNA can lengthen.
//! * **Not `ws://` or `wss://`.** Sync left the WebSocket for an SSE stream over
//!   HTTP (ADR-0023); a URL from before that is answered with its own code so
//!   a client can say what to change rather than only that it is wrong.
//!
//! What comes back is the origin's ASCII serialisation — lowercase host, IDNA
//! applied, default port dropped, no trailing slash — so two spellings of one
//! relay compare equal, which is what the pairing seam's "same relay" check
//! needs. `vectors/relay-url.json` beside this crate holds the cases, and the
//! web client's test reads the same file.

use std::net::IpAddr;

/// The longest relay URL accepted, in bytes: the pairing QR's `relay_url`
/// bound (`sunrise-pairing`'s `encode_qr_payload`).
pub const MAX_RELAY_URL_LEN: usize = 256;

/// A relay origin that passed [`parse_relay_url`]: `https://host[:port]`, or
/// `http://` to a loopback host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelayOrigin(String);

impl RelayOrigin {
    /// The origin, serialised: `https://relay.example:8443`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The origin, owned.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for RelayOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a string is not a relay URL. A code, so each client words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum RelayUrlIssue {
    /// Nothing, or only whitespace: no relay is configured.
    #[error("no relay URL is set")]
    Empty,
    /// Longer than [`MAX_RELAY_URL_LEN`] bytes, as typed or as its origin
    /// serialises.
    #[error("the relay URL is longer than 256 bytes")]
    TooLong,
    /// Not an absolute URL with a host.
    #[error("the relay URL is not a URL")]
    NotAUrl,
    /// `ws://` or `wss://`: the scheme sync used before ADR-0023.
    #[error("the relay URL is a WebSocket URL; sync is reached over https://")]
    WebSocket,
    /// Any other scheme than `http` or `https`.
    #[error("the relay URL is not https://")]
    UnsupportedScheme,
    /// `http://` to a host that is not this machine.
    #[error("the relay URL is plain http:// to another machine; use https://")]
    InsecureRemote,
    /// Carries a path, a query, a fragment or credentials.
    #[error("the relay URL has more than a scheme, host and port")]
    NotAnOrigin,
}

impl RelayUrlIssue {
    /// The issue's code, as `vectors/relay-url.json` spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::NotAUrl => "not_a_url",
            Self::WebSocket => "web_socket",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::InsecureRemote => "insecure_remote",
            Self::NotAnOrigin => "not_an_origin",
        }
    }
}

/// `input` as a relay origin, or why it is not one. See the module comment
/// for the rule.
///
/// # Errors
///
/// The [`RelayUrlIssue`] that applies, checked in the order its variants are
/// declared, except that an origin IDNA lengthens past the bound is
/// [`RelayUrlIssue::TooLong`] once every other check has passed.
pub fn parse_relay_url(input: &str) -> Result<RelayOrigin, RelayUrlIssue> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(RelayUrlIssue::Empty);
    }
    if trimmed.len() > MAX_RELAY_URL_LEN {
        return Err(RelayUrlIssue::TooLong);
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| RelayUrlIssue::NotAUrl)?;
    let loopback = match parsed.scheme() {
        "ws" | "wss" => return Err(RelayUrlIssue::WebSocket),
        "http" | "https" => match parsed.host() {
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
            Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
            None => return Err(RelayUrlIssue::NotAUrl),
        },
        _ => return Err(RelayUrlIssue::UnsupportedScheme),
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(RelayUrlIssue::NotAnOrigin);
    }
    if parsed.scheme() == "http" && !loopback {
        return Err(RelayUrlIssue::InsecureRemote);
    }
    // Checked again on what comes back: IDNA writes a non-ASCII label as
    // `xn--…`, which can be longer than the label typed, and the bound is on
    // the origin a QR carries.
    let origin = parsed.origin().ascii_serialization();
    if origin.len() > MAX_RELAY_URL_LEN {
        return Err(RelayUrlIssue::TooLong);
    }
    Ok(RelayOrigin(origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared cases: `{"input": …, "origin": …}` for an accepted URL and
    /// `{"input": …, "issue": …}` for a refused one. The web client's
    /// `server-url.test.ts` reads this same file.
    const VECTORS: &str = include_str!("../vectors/relay-url.json");

    #[test]
    fn every_shared_vector_holds() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).expect("relay-url.json");
        let cases = vectors["cases"].as_array().expect("a cases array");
        for case in cases {
            let input = case["input"].as_str().expect("an input");
            let input = input.replace("{256}", &"a".repeat(256));
            let got = parse_relay_url(&input);
            match (case["origin"].as_str(), case["issue"].as_str()) {
                (Some(origin), None) => {
                    assert_eq!(got.as_ref().map(RelayOrigin::as_str), Ok(origin), "{input}");
                }
                (None, Some(issue)) => {
                    assert_eq!(got.map_err(RelayUrlIssue::as_str), Err(issue), "{input}");
                }
                _ => panic!("a vector names an origin or an issue: {case}"),
            }
        }
        assert!(cases.len() >= 25, "only {} vectors", cases.len());
    }

    /// Two spellings of one relay are one origin, and a lookalike is not —
    /// which is what lets the pairing seam refuse a code naming another relay.
    #[test]
    fn origins_compare_by_scheme_host_and_port_alone() {
        let origin = |u: &str| parse_relay_url(u).expect("usable");
        assert_eq!(
            origin("https://Relay.Example/"),
            origin("https://relay.example")
        );
        assert_eq!(
            origin("https://relay.example:443"),
            origin("https://relay.example")
        );
        assert_ne!(
            origin("https://relay.example"),
            origin("https://relay.example.attacker.test")
        );
        assert_ne!(
            origin("https://relay.example"),
            origin("https://relay.example:8443")
        );
    }

    #[test]
    fn the_bound_is_bytes_at_the_qr_limit() {
        let host = "a".repeat(MAX_RELAY_URL_LEN - "https://.example".len());
        let at = format!("https://{host}.example");
        assert_eq!(at.len(), MAX_RELAY_URL_LEN);
        // A label over 63 bytes is still a domain to the URL parser, so the
        // bound is what refuses one byte more.
        assert!(parse_relay_url(&at).is_ok());
        assert_eq!(
            parse_relay_url(&format!("https://{host}a.example")),
            Err(RelayUrlIssue::TooLong)
        );
    }
}
