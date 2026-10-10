import Foundation

/// Where one push-token upload goes: the relay, the account's bearer, the
/// relay's id for this device, and the call that files it.
struct PushUploadTarget {
    let relayURL: String
    let bearer: String
    let relayDeviceID: String
    let upload: @MainActor (_ token: String) async throws -> Void
}

/// This device's APNs token, and whether the relay holds it.
///
/// `docs/07-clients/mobile-ios.md` §Push handling: the token goes to the relay
/// through a signed `POST /api/v1/devices/push-tokens`, again whenever it
/// changes, and again after re-pairing — which mints a new relay device id the
/// old row does not name. All three are one rule here: upload when the
/// (relay, device id, token) triple differs from the last one the relay
/// accepted.
@MainActor
final class PushTokenRegistrar {
    private static let uploadedKey = "push.uploadedRegistration"

    /// The token APNs handed this process, as lowercase hex.
    private(set) var token: String?
    private let defaults: UserDefaults
    private var uploading = false

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// Take a token from `didRegisterForRemoteNotificationsWithDeviceToken`.
    func receive(deviceToken: Data) {
        token = Self.hex(deviceToken)
    }

    /// File the token with `target` unless the relay already holds exactly
    /// this registration. A failure is not recorded, so the next call — the
    /// next token delivery or sync start — tries again.
    func uploadIfNeeded(to target: PushUploadTarget?) async {
        guard let token, let target, !uploading else { return }
        let registration = Self.fingerprint(target: target, token: token)
        guard defaults.string(forKey: Self.uploadedKey) != registration else { return }
        uploading = true
        defer { uploading = false }
        do {
            try await target.upload(token)
            defaults.set(registration, forKey: Self.uploadedKey)
        } catch {
            // Left unrecorded: the next start retries it.
        }
    }

    /// The APNs device token as the relay stores it.
    static func hex(_ token: Data) -> String {
        token.map { String(format: "%02x", $0) }.joined()
    }

    /// What decides whether an upload is owed. The bearer is not in it: a
    /// renewed token for the same account changes nothing the relay stores,
    /// and a different account means a different relay device id anyway.
    static func fingerprint(target: PushUploadTarget, token: String) -> String {
        [target.relayURL.trimmed, target.relayDeviceID.trimmed, token].joined(separator: "\n")
    }
}
