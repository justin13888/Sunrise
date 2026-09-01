import Foundation

/// Whether to start the sync driver, and with what.
///
/// A value rather than an `if` inside a view because the "no bearer" case is
/// legitimate and easy to get wrong in both directions: refusing to connect
/// without a token breaks self-host relays, and inventing an empty token
/// breaks every relay that checks one.
enum SyncPlan: Equatable {
    /// Stay local. The vault is complete on this Mac either way.
    case off(reason: String)
    /// Reach `url`, presenting `bearer` on every request.
    case connect(url: String, bearer: String?)

    init(relayURL: String, accessToken: String?) {
        let url = relayURL.trimmed
        guard !url.isEmpty else {
            self = .off(reason: "No relay is configured.")
            return
        }
        // `http`, not `ws`: ADR-0023 replaced the WebSocket with an SSE
        // stream and typed POSTs, so the relay is reached at its origin and the
        // scheme that used to be correct now names nothing this app can talk
        // to. Rejecting `ws://` explicitly rather than silently failing to
        // connect is the difference between a setting a user can fix and a
        // relay that never comes up.
        guard url.hasPrefix("http://") || url.hasPrefix("https://") else {
            let hint = url.hasPrefix("ws://") || url.hasPrefix("wss://")
                ? " Sync moved from WebSocket to HTTP; drop the /sync path too."
                : ""
            self = .off(reason: "A relay URL must start with http:// or https://.\(hint)")
            return
        }
        // An empty string is not the same as no token: the seam passes it
        // through, and a relay that checks tokens rejects the request with a
        // message about a malformed bearer rather than a missing one.
        let bearer = accessToken?.trimmed
        self = .connect(url: url, bearer: (bearer?.isEmpty ?? true) ? nil : bearer)
    }
}
