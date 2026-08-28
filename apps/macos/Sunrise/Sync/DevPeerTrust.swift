#if DEBUG
import Foundation

/// Shuttle device certificates through files, for the two-replica demo.
///
/// Two vaults sharing a root still have to trust each other's device
/// certificates before either accepts the other's op envelopes. Real pairing
/// does this over the wire; `sunrise-cli` already shuttles certs through
/// `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` for its own demo,
/// and this is the same convention on the app side so one command can drive
/// both replicas.
///
/// **Debug builds only.** A release build that trusted a peer named by an
/// environment variable would be a device-trust bypass, which is the one
/// decision in the system a user must make deliberately.
enum DevPeerTrust {
    static let exportKey = "SUNRISE_EXPORT_CERT_FILE"
    static let trustKey = "SUNRISE_TRUST_CERT_FILE"

    /// Export this device's cert and trust a peer's, if either is asked for.
    /// Returns what it did, for the log.
    @discardableResult
    static func exchange(
        bridge: CoreBridge,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) async -> [String] {
        var done: [String] = []

        if let path = environment[exportKey], !path.isEmpty {
            let cert = await bridge.deviceCertificate()
            do {
                try cert.write(to: URL(filePath: path))
                done.append("exported cert to \(path)")
            } catch {
                done.append("could not export cert: \(error.localizedDescription)")
            }
        }

        if let path = environment[trustKey], !path.isEmpty {
            do {
                let cert = try Data(contentsOf: URL(filePath: path))
                _ = try await bridge.submit(.trustDevice(certCbor: cert))
                done.append("trusted peer cert from \(path)")
            } catch {
                done.append("could not trust peer: \(error.localizedDescription)")
            }
        }

        return done
    }
}
#endif
