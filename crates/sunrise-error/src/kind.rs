//! Error category — mirrors `docs/10-cross-cutting/error-handling.md` §kind.

use serde::{Deserialize, Serialize};

/// Error category. Used to drive client retry/backoff and UX decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// Will likely succeed if retried.
    Transient,
    /// Will not succeed without intervention.
    Permanent,
    /// User input was invalid.
    User,
    /// A bug in our code.
    Internal,
}

impl ErrorKind {
    /// Stable spelling, the one `codes.toml` and the serialized form use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::User => "user",
            Self::Internal => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, in declaration order. The exhaustive match stops a new
    /// variant compiling until it is listed here too.
    fn all() -> [ErrorKind; 4] {
        let all = [
            ErrorKind::Transient,
            ErrorKind::Permanent,
            ErrorKind::User,
            ErrorKind::Internal,
        ];
        match all[0] {
            ErrorKind::Transient | ErrorKind::Permanent | ErrorKind::User | ErrorKind::Internal => {
                all
            }
        }
    }

    #[test]
    fn as_str_is_the_serde_spelling_of_every_variant() {
        for kind in all() {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::from(kind.as_str()),
            );
        }
    }
}
