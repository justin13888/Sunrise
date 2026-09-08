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
    /// Reach `url`, presenting `bearer` on every request and — when this
    /// device has registered — binding each one to `relayDeviceID`.
    case connect(url: String, bearer: String?, relayDeviceID: String?)

    /// `relayDeviceID` is the ULID the relay minted at registration, from
    /// `SessionModel.relayDeviceID`. `nil` starts an unbound driver: a
    /// self-host relay accepts one and a relay with `require_device_sig`
    /// refuses it. It is deliberately **not** a reason to stay `.off` — a
    /// client that would not connect without a binding could never reach the
    /// relay that mints it, and every self-host deployment would be unreachable
    /// besides.
    ///
    /// Defaulted, unlike `KeychainItem.accessibility`, which is required for
    /// the opposite reason. A missed protection class weakens a guarantee
    /// silently and forever; a missed binding is refused by the relay with
    /// `AUTH_DEVICE_SIG_INVALID` on the first request, which is a failure
    /// somebody sees. The default is what keeps the cases that are about URLs
    /// and bearers readable.
    init(relayURL: String, accessToken: String?, relayDeviceID: String? = nil) {
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
        // An empty id is the same trap as an empty bearer, one layer down: it
        // would put an `X-Sunrise-Device` on the wire naming no row, and the
        // relay answers that as a bad bearer so a caller cannot enumerate an
        // account's devices — which makes it undiagnosable from here.
        let device = relayDeviceID?.trimmed
        self = .connect(
            url: url,
            bearer: (bearer?.isEmpty ?? true) ? nil : bearer,
            relayDeviceID: (device?.isEmpty ?? true) ? nil : device
        )
    }
}
