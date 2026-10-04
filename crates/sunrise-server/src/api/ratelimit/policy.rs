//! The policy: which route belongs to which group, what each group and budget
//! allows, and what a request is counted against.
//!
//! `docs/06-server/api.md` §Rate limits is the same table in prose. The test
//! `every_operation_in_the_description_has_a_group` holds the two halves of
//! "every route is covered" together: a route added to the router is in the
//! description, so it fails that test until it is given a row here.

use super::bucket::Quota;
use crate::config::LimitsConfig;
use std::net::IpAddr;

/// A set of routes sharing one per-address quota.
///
/// The names are documentation only: the metric labels a refusal by the
/// matched route's own template (`endpoint`), which is finer than the group
/// and already bounded by the route table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteGroup {
    /// Liveness, readiness and the scrape: `GET /api/v1/health`, `GET /metrics`.
    Probe,
    /// `GET /api/v1/meta`.
    Meta,
    /// `POST /api/v1/accounts`, `POST /api/v1/devices`.
    Bootstrap,
    /// Account reads and device management.
    Account,
    /// The four blob routes.
    Blob,
    /// The five sync routes.
    Sync,
}

impl RouteGroup {
    /// The key prefix the group's buckets live under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Meta => "meta",
            Self::Bootstrap => "bootstrap",
            Self::Account => "account",
            Self::Blob => "blob",
            Self::Sync => "sync",
        }
    }

    /// Whether a `401` is a possible answer here, and therefore whether the
    /// failed-auth budget applies. The two unauthenticated groups never
    /// verify a credential, so there is nothing to guess at.
    #[must_use]
    pub const fn authenticates(self) -> bool {
        !matches!(self, Self::Probe | Self::Meta)
    }

    /// The per-address quota `limits` gives this group.
    #[must_use]
    pub fn quota(self, limits: &LimitsConfig) -> Quota {
        Quota::per_minute(u64::from(match self {
            Self::Probe => limits.probe_per_min,
            Self::Meta => limits.meta_per_min,
            Self::Bootstrap => limits.bootstrap_per_min,
            Self::Account => limits.account_per_min,
            Self::Blob => limits.blob_per_min,
            Self::Sync => limits.sync_per_min,
        }))
    }
}

/// The group a matched route belongs to, by its description path and verb.
///
/// `None` for a route this table does not name. The interceptor then applies
/// the strictest authenticated group rather than nothing, and the test below
/// fails, so an unlisted route is a red build rather than an unlimited one.
///
/// No `OPTIONS` row: the CORS preflight kynos synthesizes carries no
/// interceptors, so `Admission` never sees one and a row would never be read.
/// `docs/06-server/api.md` §What this does not cover records the gap.
#[must_use]
pub fn route_group(method: &str, path: &str) -> Option<RouteGroup> {
    use RouteGroup::{Account, Blob, Bootstrap, Meta, Probe, Sync};
    Some(match (method, path) {
        ("GET", "/api/v1/health" | "/metrics") => Probe,
        ("GET", "/api/v1/meta") => Meta,
        ("POST", "/api/v1/accounts" | "/api/v1/devices") => Bootstrap,
        (
            "GET",
            "/api/v1/accounts/me" | "/api/v1/accounts/me/recovery_blob" | "/api/v1/devices",
        )
        | (
            "DELETE",
            "/api/v1/devices/{device_id}" | "/api/v1/devices/by-vault-id/{vault_device_id}",
        )
        | ("POST", "/api/v1/devices/push-tokens") => Account,
        ("POST", "/api/v1/blobs/init" | "/api/v1/blobs/finalize")
        | ("PUT", "/api/v1/blobs/{upload_id}/{chunk_idx}")
        | ("GET", "/api/v1/blobs/{blob_id}") => Blob,
        (
            "POST",
            "/api/v1/sync/session"
            | "/api/v1/sync/session/refresh"
            | "/api/v1/sync/subscribe"
            | "/api/v1/sync/ops",
        )
        | ("GET", "/api/v1/sync/events") => Sync,
        _ => return None,
    })
}

/// The group an unlisted route is held to: the tightest authenticated one, so
/// a route nobody classified is never the loosest door in.
pub const UNLISTED: RouteGroup = RouteGroup::Bootstrap;

/// The failed-auth budget: `401`s per client address per five minutes.
#[must_use]
pub fn failed_auth_quota(limits: &LimitsConfig) -> Quota {
    Quota::per_secs(u64::from(limits.failed_auth_per_5min), 300)
}

/// The per-device and per-account budgets the handlers charge after
/// authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Ops on `POST /sync/ops`, per device.
    Ops,
    /// Chunk bytes on `PUT /blobs/{upload_id}/{chunk_idx}`, per device.
    BlobUpload,
    /// Blob bytes on `GET /blobs/{blob_id}`, per device.
    BlobDownload,
    /// New sync sessions, per device.
    Sessions,
}

impl Budget {
    /// The key prefix this budget's buckets live under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ops => "ops",
            Self::BlobUpload => "blob_up",
            Self::BlobDownload => "blob_down",
            Self::Sessions => "sessions",
        }
    }

    /// The quota `limits` gives this budget.
    ///
    /// Ops burst to ten seconds' worth: `backpressure-and-quotas.md` states
    /// the rate as "averaged over a 10 s window", and a reconnecting client
    /// drains its outbox in a burst.
    #[must_use]
    pub fn quota(self, limits: &LimitsConfig) -> Quota {
        match self {
            Self::Ops => Quota::per_secs(u64::from(limits.ops_per_sec) * 10, 10),
            Self::BlobUpload => Quota::per_minute(limits.blob_upload_bytes_per_min),
            Self::BlobDownload => Quota::per_minute(limits.blob_download_bytes_per_min),
            Self::Sessions => Quota::per_secs(u64::from(limits.sessions_per_5min), 300),
        }
    }
}

/// The bucket key for a client address.
///
/// IPv4 by whole address. IPv6 by its `/64`: one subscriber is routinely
/// handed a whole `/64`, so keying on the full address would give a single
/// client 2^64 buckets to rotate through.
#[must_use]
pub fn address_key(client: Option<IpAddr>) -> String {
    match client.map(canonical) {
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        // An in-process request with no socket. One shared bucket, never an
        // exemption: a limit that silently does not apply is worse than none.
        None => "none".to_owned(),
    }
}

/// What a log line may say about a client address: its `/24` or `/48`.
///
/// `docs/10-cross-cutting/logging.md` §6.2 allows those prefixes and nothing
/// finer, which is enough to tell two clients apart in a refusal log and not
/// enough to name a household.
#[must_use]
pub fn address_net(client: Option<IpAddr>) -> String {
    match client.map(canonical) {
        Some(IpAddr::V4(v4)) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        Some(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
        None => "none".to_owned(),
    }
}

/// An IPv4 client reached over a dual-stack socket arrives as `::ffff:a.b.c.d`;
/// it is the IPv4 client, and must share that client's bucket.
fn canonical(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every route is in the policy.** The description is generated from
    /// the router, so a route added later appears here without anyone
    /// remembering to add it, and fails until it is given a group.
    #[test]
    fn every_operation_in_the_description_has_a_group() {
        let doc = crate::api::document().expect("the router must describe");
        let v: serde_json::Value =
            serde_json::from_str(&doc.to_json().expect("serializes")).expect("valid JSON");
        let paths = v["paths"].as_object().expect("paths");
        let mut seen = 0;
        for (path, item) in paths {
            for (method, _) in item.as_object().expect("a path item") {
                if !["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                    continue;
                }
                seen += 1;
                assert!(
                    route_group(&method.to_uppercase(), path).is_some(),
                    "{} {path} has no row in the rate-limit policy \
                     (api/ratelimit/policy.rs and docs/06-server/api.md §Rate limits)",
                    method.to_uppercase()
                );
            }
        }
        assert!(seen >= 19, "the description lost its operations: {seen}");
    }

    /// `/metrics` is only in the description on a loopback bind, which is the
    /// default the test above reads; its row is pinned here directly.
    #[test]
    fn the_scrape_is_a_probe() {
        assert_eq!(route_group("GET", "/metrics"), Some(RouteGroup::Probe));
    }

    #[test]
    fn an_unlisted_route_has_no_group() {
        assert_eq!(route_group("GET", "/api/v1/admin/stats"), None);
        assert_eq!(route_group("POST", "/api/v1/meta"), None, "the verb counts");
    }

    #[test]
    fn ipv6_clients_share_a_bucket_per_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(address_key(Some(a)), address_key(Some(b)));
        assert_ne!(address_key(Some(a)), address_key(Some(c)));
    }

    #[test]
    fn an_ipv4_mapped_client_is_the_ipv4_client() {
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(address_key(Some(mapped)), address_key(Some(v4)));
        assert_eq!(address_net(Some(mapped)), "203.0.113.0/24");
    }

    #[test]
    fn the_logged_network_is_never_finer_than_24_or_48() {
        assert_eq!(
            address_net(Some("198.51.100.23".parse().unwrap())),
            "198.51.100.0/24"
        );
        assert_eq!(
            address_net(Some("2001:db8:abcd:12::1".parse().unwrap())),
            "2001:db8:abcd::/48"
        );
        assert_eq!(address_net(None), "none");
    }

    /// The ops budget is the rate `backpressure-and-quotas.md` states,
    /// averaged over its ten-second window.
    #[test]
    fn the_ops_budget_is_fifty_a_second_over_ten_seconds() {
        let q = Budget::Ops.quota(&LimitsConfig::default());
        assert_eq!(q, Quota::per_secs(500, 10));
    }
}
