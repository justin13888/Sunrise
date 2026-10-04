//! The `[limits]` table, and the trusted-proxy list the per-address limits
//! depend on.
//!
//! `docs/06-server/api.md` §Rate limits is the policy these numbers implement:
//! one row per route group, and the per-device and per-account budgets the
//! handlers charge after authentication. Every default here is that table's
//! default, so a self-hoster who writes no `[limits]` table runs the documented
//! policy.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Every rate limit the relay enforces.
///
/// Per-address rows are per minute, the unit the policy table is written in.
/// The weighted budgets are in the unit their cost is counted in: ops, or
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// `false` turns every limit off. For a benchmark or a load test, never a
    /// public relay.
    pub enabled: bool,
    /// `GET /api/v1/health` and `GET /metrics`, per client address.
    pub probe_per_min: u32,
    /// `GET /api/v1/meta`, per client address.
    pub meta_per_min: u32,
    /// `POST /api/v1/accounts` and `POST /api/v1/devices`, per client address.
    pub bootstrap_per_min: u32,
    /// The account and device management routes, per client address.
    pub account_per_min: u32,
    /// The four blob routes, per client address.
    pub blob_per_min: u32,
    /// The five sync routes, per client address.
    pub sync_per_min: u32,
    /// `401`s per client address per five minutes. Past it, a request to an
    /// authenticated route is refused before its bearer is verified.
    pub failed_auth_per_5min: u32,
    /// Ops per device per second on `POST /api/v1/sync/ops`, bursting to ten
    /// seconds' worth.
    pub ops_per_sec: u32,
    /// Chunk bytes per device per minute on `PUT /api/v1/blobs/{..}/{..}`.
    pub blob_upload_bytes_per_min: u64,
    /// Blob bytes per device per minute on `GET /api/v1/blobs/{blob_id}`.
    pub blob_download_bytes_per_min: u64,
    /// Uploads one account may have open at once, between `init` and
    /// `finalize`.
    pub open_uploads: u32,
    /// New sync sessions per device per five minutes.
    pub sessions_per_5min: u32,
    /// Event streams one device may hold open at once.
    pub streams: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            probe_per_min: 120,
            meta_per_min: 60,
            bootstrap_per_min: 10,
            account_per_min: 60,
            blob_per_min: 600,
            sync_per_min: 600,
            failed_auth_per_5min: 20,
            ops_per_sec: 50,
            blob_upload_bytes_per_min: 64 * 1024 * 1024,
            blob_download_bytes_per_min: 256 * 1024 * 1024,
            open_uploads: 16,
            sessions_per_5min: 10,
            streams: 4,
        }
    }
}

impl LimitsConfig {
    /// Every limit off, for tests that exercise something else at volume.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The first limit set to zero, by its key name.
    ///
    /// Zero would refuse every request it covers, which is a relay that boots
    /// and serves nothing; `enabled = false` is how a limit is turned off.
    #[must_use]
    pub fn first_zero(&self) -> Option<&'static str> {
        let rows: [(&'static str, u64); 13] = [
            ("probe_per_min", self.probe_per_min.into()),
            ("meta_per_min", self.meta_per_min.into()),
            ("bootstrap_per_min", self.bootstrap_per_min.into()),
            ("account_per_min", self.account_per_min.into()),
            ("blob_per_min", self.blob_per_min.into()),
            ("sync_per_min", self.sync_per_min.into()),
            ("failed_auth_per_5min", self.failed_auth_per_5min.into()),
            ("ops_per_sec", self.ops_per_sec.into()),
            ("blob_upload_bytes_per_min", self.blob_upload_bytes_per_min),
            (
                "blob_download_bytes_per_min",
                self.blob_download_bytes_per_min,
            ),
            ("open_uploads", self.open_uploads.into()),
            ("sessions_per_5min", self.sessions_per_5min.into()),
            ("streams", self.streams.into()),
        ];
        rows.into_iter().find(|(_, v)| *v == 0).map(|(k, _)| k)
    }
}

/// Parse one `trusted_proxies` entry: an address, or an address and a prefix
/// length (`10.0.0.0/8`, `fd00::/8`).
///
/// A bare address is its own single-host network. The bits past the prefix
/// are not required to be zero, because `10.1.2.3/8` names the same network
/// as `10.0.0.0/8` and refusing it would be pedantry about a typo with one
/// meaning.
#[must_use]
pub fn parse_cidr(entry: &str) -> Option<(IpAddr, u8)> {
    let entry = entry.trim();
    let (addr, prefix) = match entry.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (entry, None),
    };
    let addr: IpAddr = addr.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
        None => max,
    };
    Some((addr, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_carry_no_zero() {
        assert_eq!(LimitsConfig::default().first_zero(), None);
    }

    #[test]
    fn a_zero_is_named_by_its_key() {
        let limits = LimitsConfig {
            streams: 0,
            ..LimitsConfig::default()
        };
        assert_eq!(limits.first_zero(), Some("streams"));
    }

    #[test]
    fn cidrs_parse_with_and_without_a_prefix() {
        assert_eq!(
            parse_cidr("10.0.0.0/8"),
            Some(("10.0.0.0".parse().unwrap(), 8))
        );
        assert_eq!(
            parse_cidr("127.0.0.1"),
            Some(("127.0.0.1".parse().unwrap(), 32))
        );
        assert_eq!(parse_cidr("fd00::/8"), Some(("fd00::".parse().unwrap(), 8)));
        assert_eq!(parse_cidr("::1"), Some(("::1".parse().unwrap(), 128)));
    }

    #[test]
    fn a_malformed_cidr_is_refused() {
        for bad in [
            "",
            "10.0.0.0/33",
            "::/129",
            "10.0.0.0/",
            "proxy.local",
            "10.0.0/8",
            "*",
        ] {
            assert_eq!(parse_cidr(bad), None, "{bad:?} must not parse");
        }
    }
}
