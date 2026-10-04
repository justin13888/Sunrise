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

    /// The plan for `relayDeviceID` as `SessionModel.relayDeviceID` answers
    /// it: an id, no id, or a Keychain that was reached and refused (#284).
    ///
    /// The first two are the plan above. The refusal is **off**, and not the
    /// unbound driver the default's reasoning covers: that reasoning holds for
    /// a device that never registered, where the relay's refusal is the signal
    /// and registering is the remedy. Here the id may exist and the store
    /// would not say, so an unbound driver either drops the ADR-0022 binding
    /// for the life of the process on a relay that tolerates it, or is refused
    /// as `AUTH_DEVICE_SIG_INVALID` — a message about a signature — for a
    /// condition an unlock fixes. The reason names the Keychain instead, and
    /// the next sync start reads it again. A plan that is already off for a
    /// relay reason keeps that reason: it is the one the user can act on first.
    init(relayURL: String, accessToken: String?, relayDeviceID: Result<String?, any Error>) {
        switch relayDeviceID {
        case let .success(id):
            self.init(relayURL: relayURL, accessToken: accessToken, relayDeviceID: id)
        case let .failure(refusal):
            let plan = SyncPlan(relayURL: relayURL, accessToken: accessToken)
            guard case .connect = plan else {
                self = plan
                return
            }
            self = .off(
                reason: "Sync is off because the Keychain would not read this device's relay id: "
                    + refusal.localizedDescription
            )
        }
    }
}
