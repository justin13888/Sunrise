//! The relay URL rule, lowered onto UniFFI:
//! [`sunrise_client_core::relay_url`], the one rule every client shares (the
//! web client is tested against the same vectors).

use sunrise_client_core::relay_url::{self, RelayUrlIssue};

/// See [`sunrise_client_core::relay_url::RelayUrlIssue`].
#[uniffi::remote(Enum)]
pub enum RelayUrlIssue {
    Empty,
    TooLong,
    NotAUrl,
    WebSocket,
    UnsupportedScheme,
    InsecureRemote,
    NotAnOrigin,
}

/// What [`check_relay_url`] made of a string.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum RelayUrlCheck {
    /// A relay origin: `https://relay.example:8443`, no trailing slash.
    Origin {
        /// The origin, serialised.
        origin: String,
    },
    /// Not one, and why.
    Refused {
        /// Why.
        issue: RelayUrlIssue,
    },
}

/// Whether `input` is a relay URL, by the one rule every client shares (the
/// web client reads the same vectors): `https://`, or `http://` to a loopback
/// host, an origin and nothing more, at most 256 bytes.
#[uniffi::export]
#[must_use]
pub fn check_relay_url(input: String) -> RelayUrlCheck {
    match relay_url::parse_relay_url(&input) {
        Ok(origin) => RelayUrlCheck::Origin {
            origin: origin.into_string(),
        },
        Err(issue) => RelayUrlCheck::Refused { issue },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_lowers_the_client_layer_unchanged() {
        assert_eq!(
            check_relay_url("https://Relay.Example/".into()),
            RelayUrlCheck::Origin {
                origin: "https://relay.example".into()
            }
        );
        assert_eq!(
            check_relay_url("http://relay.example".into()),
            RelayUrlCheck::Refused {
                issue: RelayUrlIssue::InsecureRemote
            }
        );
    }
}
