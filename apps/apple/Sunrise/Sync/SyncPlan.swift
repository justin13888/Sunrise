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
    /// Dial `url`, presenting `bearer` on the upgrade.
    case connect(url: String, bearer: String?)

    init(relayURL: String, accessToken: String?) {
        let url = relayURL.trimmed
        guard !url.isEmpty else {
            self = .off(reason: "No relay is configured.")
            return
        }
        guard url.hasPrefix("ws://") || url.hasPrefix("wss://") else {
            self = .off(reason: "A relay URL must start with ws:// or wss://.")
            return
        }
        // An empty string is not the same as no token: the seam passes it
        // through, and a relay that checks tokens rejects the upgrade with a
        // message about a malformed bearer rather than a missing one.
        let bearer = accessToken?.trimmed
        self = .connect(url: url, bearer: (bearer?.isEmpty ?? true) ? nil : bearer)
    }
}
