//! What a host hands the core when it opens a vault: a clock and a root.
//!
//! The core never reads an ambient clock (`clippy.toml`'s determinism rules,
//! `docs/01-architecture/shared-core.md`), and it is never handed a root of
//! the wrong width. Both are checks every client makes at its own seam — the
//! `UniFFI` seam on bytes from the Keychain, the web core on bytes from
//! JavaScript — so they are made here, once.

use sunrise_core::Clock;

/// A wall clock read from the host, with the host's zone fixed when it is
/// built.
///
/// `now` is a plain function so a browser build can pass `Date.now()` and a
/// test can pass a constant. A zone name the core's bundled tzdb does not know
/// evaluates as UTC there, the fallback every native clock gets too.
#[derive(Debug, Clone)]
pub struct HostClock {
    now: fn() -> u64,
    timezone: String,
}

impl HostClock {
    /// A clock reading `now` and reporting `timezone`, an IANA name.
    #[must_use]
    pub fn new(now: fn() -> u64, timezone: impl Into<String>) -> Self {
        Self {
            now,
            timezone: timezone.into(),
        }
    }
}

impl Clock for HostClock {
    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    fn timezone(&self) -> String {
        self.timezone.clone()
    }
}

/// The width a vault root must have.
pub const VAULT_ROOT_LEN: usize = 32;

/// A vault root that was not [`VAULT_ROOT_LEN`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("vault root must be 32 bytes, got {len}")]
pub struct BadVaultRoot {
    /// The length the host passed.
    pub len: usize,
}

/// A vault root as it crossed a seam, checked for its width.
///
/// # Errors
/// [`BadVaultRoot`] carrying the length that was passed.
pub fn vault_root(bytes: &[u8]) -> Result<[u8; VAULT_ROOT_LEN], BadVaultRoot> {
    bytes
        .try_into()
        .map_err(|_| BadVaultRoot { len: bytes.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> u64 {
        1_767_225_600_000
    }

    #[test]
    fn the_host_clock_reports_what_the_host_gave_it() {
        let clock = HostClock::new(fixed_now, "America/Toronto");
        assert_eq!(clock.timezone(), "America/Toronto");
        assert_eq!(clock.now_ms(), fixed_now());
    }

    #[test]
    fn a_root_is_exactly_thirty_two_bytes() {
        assert_eq!(vault_root(&[7; 32]), Ok([7; 32]));
        assert_eq!(vault_root(&[7; 31]), Err(BadVaultRoot { len: 31 }));
        assert_eq!(vault_root(&[]), Err(BadVaultRoot { len: 0 }));
        assert_eq!(
            vault_root(&[7; 33]).unwrap_err().to_string(),
            "vault root must be 32 bytes, got 33"
        );
    }
}
