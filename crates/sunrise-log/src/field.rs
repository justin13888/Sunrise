//! The log-field allowlist — the redaction rule, expressed as data.
//!
//! Per `docs/10-cross-cutting/logging.md` §6, a Sunrise log record may only
//! carry context under keys drawn from a fixed allowlist. Everything else is
//! assumed to be plaintext user data until proven otherwise, and is refused.
//!
//! `tracing` supplies the transport, the levels, and the values; this module
//! supplies the *vocabulary*. [`crate::RedactionLayer`] enforces it: an event
//! from a `sunrise_*` target carrying a field name outside [`ALLOWED`] never
//! reaches a subscriber.
//!
//! Adding a key here is a deliberate act — it asserts that values under that
//! name can never be user-authored content.

/// Field names permitted on events emitted from `sunrise_*` targets.
///
/// **Sorted**; [`is_allowed`] binary-searches it and a unit test asserts the
/// ordering, so insertions must keep it sorted.
///
/// Grouped by role in the comments below; the array itself is one flat sorted
/// run because that is what makes the lookup cheap.
pub static ALLOWED: &[&str] = &[
    // --- per-device entity hashes: `BLAKE3(id || device_log_salt, 4)` hex ---
    "account_h",
    // --- structural enums (never content) ---
    "action_kind",
    "aead_alg",
    // --- versions / build identity ---
    "app_v",
    "attachment_h",
    // --- counters, sizes, latencies ---
    "attempt",
    // The sender's per-session outbox batch counter, not an entity id. It is
    // minted by the client, restarts at 1 on every reconnect, and names
    // nothing outside one live session — which is also why the relay refuses
    // to dedup on it. Carved out of the `_id` rule by `NOT_ENTITY_IDS`.
    "batch_id",
    // Server's own listen address from operator config. Not a client IP:
    // §6.2 forbids logging peer addresses, not the socket we opened.
    "bind",
    "block_h",
    "cause",
    "crypto_v",
    "delay_ms",
    "device_h",
    "doc_v",
    // --- redaction-layer self-reporting (see `RedactionLayer`) ---
    "dropped_field",
    "dropped_target",
    // Templated request target — see [`templatize_path`]. Never a raw URI.
    "endpoint",
    "epoch",
    // --- error envelope (logging.md §5) ---
    "err_code",
    "err_kind",
    // The hierarchical event name (logging.md §3). Validated by
    // [`crate::EventName`].
    "ev",
    // --- server-minted wall-clock instants (machine time, never authored) ---
    // The deadline the relay adopted for a refreshed bearer, and the relay's
    // own arrival stamp for an op batch. Both are the server's clock talking
    // about itself; neither is derived from anything a user typed.
    "expires_at_ms",
    "first_seen_ms",
    "from_v",
    "kind",
    "lat_ms",
    // `message` is tracing's name for the format-string body. It is
    // allowlisted because every event has one; keeping it free of plaintext
    // is the job of the `.expose()` CI gate, not of this list.
    "message",
    "method",
    "mode",
    "n_bytes",
    "n_chunks",
    "n_devices",
    "n_dropped",
    "n_exported",
    "n_imported",
    "n_ops",
    "n_retained",
    "n_streams",
    "note_h",
    "op_kind",
    "person_h",
    "provider",
    // Relay *hostname* — §6.2 names this as the sanctioned stand-in for a
    // client's own IP in connection diagnostics.
    "relay",
    "result",
    "retryable",
    "routine_h",
    "seq",
    "sig_alg",
    "status",
    "storage_v",
    "stream_h",
    "task_h",
    "tier",
    "to_v",
    "view",
    "wire_v",
];

/// Whether `name` may appear as a field on a Sunrise log event.
#[must_use]
pub fn is_allowed(name: &str) -> bool {
    ALLOWED.binary_search(&name).is_ok()
}

/// Rewrite a request target into something loggable.
///
/// Two things happen, and both are load-bearing:
///
/// 1. **The query string is dropped entirely.** Browsers that cannot set an
///    `Authorization` header pass `?access_token=…`; a request log that keeps
///    the query is a bearer-token log.
/// 2. **Opaque path segments are replaced with `:id`.** `/api/v1/devices/
///    01J8…` carries a full device id, which §6 lists as a stable identifier
///    that must never be logged. What is useful for debugging is the *route*,
///    so that is what survives.
///
/// A segment is opaque if it is at least 12 bytes and consists only of
/// `[0-9A-Za-z_-]` with at least one digit — which catches Crockford base-32
/// ULIDs, hex ids, and base64url tokens while leaving real route words
/// (`devices`, `accounts`, `blobs`) alone.
#[must_use]
pub fn templatize_path(target: &str) -> String {
    let path = target.split(['?', '#']).next().unwrap_or("");
    let mut out = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if is_opaque_segment(seg) {
            out.push_str(":id");
        } else {
            out.push_str(seg);
        }
    }
    out
}

fn is_opaque_segment(seg: &str) -> bool {
    seg.len() >= 12
        && seg.bytes().any(|b| b.is_ascii_digit())
        && seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_sorted_and_unique() {
        // `is_allowed` binary-searches, so an unsorted entry would silently
        // become unreachable rather than fail loudly.
        let mut sorted = ALLOWED.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ALLOWED, "ALLOWED must stay sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), ALLOWED.len(), "ALLOWED has duplicates");
    }

    #[test]
    fn allowlist_names_are_snake_case() {
        for name in ALLOWED {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "field {name:?} is not [a-z0-9_]"
            );
        }
    }

    #[test]
    fn known_keys_allowed_unknown_keys_refused() {
        assert!(is_allowed("stream_h"));
        assert!(is_allowed("lat_ms"));
        assert!(is_allowed("ev"));
        assert!(is_allowed("message"));
        // The shapes a leak actually takes.
        assert!(!is_allowed("title"));
        assert!(!is_allowed("body"));
        assert!(!is_allowed("email"));
        assert!(!is_allowed("task_id"));
        assert!(!is_allowed("device_id"));
        assert!(!is_allowed("token"));
        assert!(!is_allowed("query"));
        assert!(!is_allowed("uri"));
    }

    /// Names ending `_id` that are not entity identifiers.
    ///
    /// The suffix rule is worth keeping literal, so its exceptions are listed
    /// one by one rather than pattern-matched: each has to be argued for by
    /// hand, which is the point of the rule.
    const NOT_ENTITY_IDS: &[&str] = &["batch_id"];

    #[test]
    fn no_raw_identifier_keys_on_the_allowlist() {
        // §6 bans full ids; only the `_h` hashes may appear. This catches a
        // future edit that adds `task_id` next to `task_h`.
        for name in ALLOWED {
            if NOT_ENTITY_IDS.contains(name) {
                continue;
            }
            assert!(
                !name.ends_with("_id"),
                "field {name:?} looks like a raw identifier"
            );
        }
    }

    #[test]
    fn the_id_carve_out_is_not_a_loophole() {
        // A carve-out for a name that is not on the list would quietly widen
        // the rule the next time someone added that name.
        for name in NOT_ENTITY_IDS {
            assert!(
                ALLOWED.contains(name),
                "stale carve-out {name:?}: not on the allowlist"
            );
        }
        // And the shapes the rule exists to catch stay caught.
        for leak in ["task_id", "device_id", "person_id", "account_id"] {
            assert!(!NOT_ENTITY_IDS.contains(&leak), "{leak:?} is an entity id");
            assert!(!is_allowed(leak), "{leak:?} must never be allowlisted");
        }
    }

    #[test]
    fn templatize_drops_query_string() {
        assert_eq!(
            templatize_path("/sync?access_token=eyJhbGciOiJIUzI1NiJ9.secret"),
            "/sync"
        );
        assert_eq!(templatize_path("/api/v1/meta#frag"), "/api/v1/meta");
    }

    #[test]
    fn templatize_replaces_opaque_segments() {
        assert_eq!(
            templatize_path("/api/v1/devices/01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
            "/api/v1/devices/:id"
        );
        assert_eq!(
            templatize_path("/api/v1/blobs/a3f9c1d20b4e8867"),
            "/api/v1/blobs/:id"
        );
    }

    #[test]
    fn templatize_keeps_route_words() {
        assert_eq!(templatize_path("/api/v1/accounts"), "/api/v1/accounts");
        assert_eq!(templatize_path("/healthz"), "/healthz");
        assert_eq!(templatize_path("/"), "/");
        // Long but alphabetic: a route word, not an id.
        assert_eq!(
            templatize_path("/api/v1/notifications"),
            "/api/v1/notifications"
        );
    }
}
