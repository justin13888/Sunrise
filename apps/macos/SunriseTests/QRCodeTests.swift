import Foundation
import Testing

@testable import Sunrise

/// The QR renderer.
///
/// `docs/07-clients/parity-matrix.md` makes "Pairing — show QR" a MUST for
/// this client, and until this existed there was no QR encoder in the app at
/// all. The tests below are cheap, and they cover the two ways a pairing
/// screen fails silently: drawing nothing, and drawing something too small to
/// scan.
struct QRCodeTests {
    /// A payload the size of a real one. `DevicePairing.offer` produces the
    /// same JSON shape, so this is what the screen actually has to encode.
    private func payload() throws -> String {
        let pairing = try DevicePairing.offer(
            relayUrl: "wss://relay.example/sync",
            accountEmail: "someone@example.com"
        )
        return try #require(pairing.qrPayload())
    }

    @Test
    func aRealPairingPayloadRendersAsACode() throws {
        let image = try #require(QRCode.image(for: try payload()))

        #expect(image.size.width > 0)
        #expect(image.size.width == image.size.height, "a QR symbol is square")
    }

    /// The generator emits one pixel per module. Scaled up by a whole number
    /// of pixels the modules stay hard-edged; scaled by a fraction, or not at
    /// all, a camera has nothing to lock onto.
    @Test
    func theSymbolIsScaledUpToSomethingACameraCanRead() throws {
        let small = try #require(QRCode.image(for: try payload(), side: 240))
        let large = try #require(QRCode.image(for: try payload(), side: 480))

        #expect(small.size.width >= 100)
        #expect(large.size.width > small.size.width)
        #expect(small.size.width.truncatingRemainder(dividingBy: 1) == 0)
    }

    @Test
    func aRequestWithNothingToEncodeDrawsNothing() {
        #expect(QRCode.image(for: "") == nil)
        #expect(QRCode.image(for: "anything", side: 0) == nil)
    }

    /// Same input, same symbol. Not a property of QR so much as of this
    /// wrapper: a renderer that produced a different image each call would
    /// repaint the pairing screen under the user mid-scan.
    @Test
    func theSameTextAlwaysDrawsTheSameSymbol() throws {
        let text = try payload()
        let first = try #require(QRCode.image(for: text))
        let second = try #require(QRCode.image(for: text))

        #expect(first.size == second.size)
        #expect(first.tiffRepresentation == second.tiffRepresentation)
    }
}
