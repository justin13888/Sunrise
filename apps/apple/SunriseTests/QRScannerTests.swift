import CoreGraphics
import Foundation
import Testing

@testable import Sunrise

/// The scanner's reader, fed by a stub capture source instead of a camera
/// (#464).
///
/// A camera cannot run in a test, but everything after it can: the frame goes
/// to ``QRDecoder``, and what the decoder reads goes to the pairing model as
/// text. So the stub here is a still picture of a real pairing code, drawn by
/// the same ``QRCode`` the pairing screen draws with, and the claim is the one
/// a camera would make — the symbol on one device's screen becomes the other
/// device's pairing.
@MainActor
struct QRScannerTests {
    private static let relay = "https://relay.example"

    private func realPayload() throws -> String {
        let pairing = try DevicePairing.offer(relayUrl: Self.relay, accountEmail: "someone@example.com")
        return try #require(pairing.qrPayload())
    }

    /// A blank white frame: what a camera sees pointed at nothing.
    private func blankFrame() throws -> CGImage {
        let context = try #require(
            CGContext(
                data: nil,
                width: 64,
                height: 64,
                bitsPerComponent: 8,
                bytesPerRow: 0,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            )
        )
        context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: 64, height: 64))
        return try #require(context.makeImage())
    }

    /// What the pairing screen draws is what the scanner reads, byte for byte.
    @Test
    func aDrawnPairingCodeReadsBackAsTheSamePayload() throws {
        let payload = try realPayload()
        let image = try #require(QRCode.cgImage(for: payload))
        #expect(QRDecoder.firstPayload(in: image) == payload)
    }

    /// A frame with no code in it is nothing, not an error and not a guess.
    @Test
    func aFrameWithNoCodeReadsAsNothing() throws {
        #expect(QRDecoder.firstPayload(in: try blankFrame()) == nil)
    }

    /// A scanned code starts the pairing exactly as a pasted one does: the
    /// scanner's only output is the text the model would otherwise have been
    /// handed by the paste field.
    @Test
    func aScannedCodeStartsThePairingTheWayAPasteDoes() async throws {
        let payload = try realPayload()
        let frame = try #require(QRCode.cgImage(for: payload))
        let scanned = try #require(QRDecoder.firstPayload(in: frame))

        let holder = PairingModel(intent: .addAnotherDevice)
        holder.pasted = scanned
        await holder.submit()

        #expect(holder.role == .existingDevice, "the seam accepted the scanned code")
        guard case let .awaiting(prompt) = holder.phase else {
            Issue.record("expected the next paste, got \(holder.phase)")
            return
        }
        #expect(prompt.leg == .first, "and the pairing moved past the code")
    }

    /// The harness's two scanner switches, and that neither reaches a launch
    /// without a scratch vault.
    @Test
    func theHarnessInjectsACodeOrARefusalOnlyIntoAUITestLaunch() throws {
        let vault = ["-sunrise-ui-test-vault", "/tmp/scratch"]
        #expect(UITestHarness.scanSource(arguments: [UITestHarness.scanFixtureFlag]) == nil)
        #expect(UITestHarness.scanSource(arguments: vault) == nil, "the camera, unless a test asks")

        let refused = UITestHarness.scanSource(arguments: vault + [UITestHarness.cameraRefusedFlag])
        guard case .refused = refused else {
            Issue.record("the refusal flag stands in for the system's answer")
            return
        }
        let fixture = UITestHarness.scanSource(arguments: vault + [UITestHarness.scanFixtureFlag])
        guard case let .still(image) = fixture else {
            Issue.record("the fixture flag puts a picture in front of the scanner")
            return
        }
        let payload = try #require(QRDecoder.firstPayload(in: image))
        #expect(throws: Never.self, "and it is a code the seam accepts") {
            _ = try DevicePairing.accept(qrPayload: payload)
        }
    }
}
