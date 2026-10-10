import Foundation
import Testing

@testable import Sunrise

private struct RelayRefused: Error {}

@MainActor
struct PushTokenRegistrarTests {
    private func defaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// What reached the relay, in order.
    @MainActor
    private final class Relay {
        var filed: [(device: String, token: String)] = []
        var refuses = false

        func target(device: String, relayURL: String = "https://relay.example") -> PushUploadTarget {
            PushUploadTarget(relayURL: relayURL, bearer: "bearer", relayDeviceID: device) { token in
                if self.refuses { throw RelayRefused() }
                self.filed.append((device, token))
            }
        }
    }

    /// The relay stores the token as APNs hands it to the provider: lowercase
    /// hex of the raw bytes.
    @Test
    func theTokenIsLowercaseHex() {
        #expect(PushTokenRegistrar.hex(Data([0x00, 0xAB, 0x7F, 0xFF])) == "00ab7fff")
    }

    @Test
    func aTokenIsFiledOnceForTheSameDevice() async throws {
        let registrar = PushTokenRegistrar(defaults: try defaults())
        let relay = Relay()
        registrar.receive(deviceToken: Data([0x01, 0x02]))

        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))

        #expect(relay.filed.map(\.token) == ["0102"])
    }

    /// A rotated token, and a re-pairing's new relay device id, are both a
    /// registration the relay does not hold yet.
    @Test
    func aRotationOrARePairingIsFiledAgain() async throws {
        let registrar = PushTokenRegistrar(defaults: try defaults())
        let relay = Relay()
        registrar.receive(deviceToken: Data([0x01]))
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))

        registrar.receive(deviceToken: Data([0x02]))
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV2"))

        #expect(relay.filed.map(\.device) == ["DEV1", "DEV1", "DEV2"])
        #expect(relay.filed.map(\.token) == ["01", "02", "02"])
    }

    /// A refusal is not remembered as success, so the next sync start tries
    /// again.
    @Test
    func aRefusedUploadIsRetried() async throws {
        let registrar = PushTokenRegistrar(defaults: try defaults())
        let relay = Relay()
        registrar.receive(deviceToken: Data([0x0F]))

        relay.refuses = true
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))
        relay.refuses = false
        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))

        #expect(relay.filed.map(\.token) == ["0f"])
    }

    /// Nothing to send, or nowhere to send it: no call.
    @Test
    func noTokenOrNoTargetUploadsNothing() async throws {
        let registrar = PushTokenRegistrar(defaults: try defaults())
        let relay = Relay()

        await registrar.uploadIfNeeded(to: relay.target(device: "DEV1"))
        registrar.receive(deviceToken: Data([0x01]))
        await registrar.uploadIfNeeded(to: nil)

        #expect(relay.filed.isEmpty)
    }
}
