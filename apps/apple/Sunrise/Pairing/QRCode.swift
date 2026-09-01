import CoreGraphics
import CoreImage
import CoreImage.CIFilterBuiltins
import Foundation

/// Draws a pairing payload as a QR code.
///
/// CoreImage rather than a QR library: `CIQRCodeGenerator` is in the OS, it
/// implements the same ISO 18004 the spec's payload assumes, and a third-party
/// encoder here would be a dependency whose only job is to reproduce it.
///
/// The generator emits one pixel per module, which renders as a blurred smear
/// at any useful size — so the output is scaled with nearest-neighbour before
/// it becomes an image. That is not cosmetic: a camera on the other device
/// reads the modules, and interpolated edges are what makes a code refuse to
/// scan.
enum QRCode {
    /// Error correction level. `M` — 15% — is what leaves room for a phone
    /// camera at an angle without pushing a ~200-byte payload into a denser
    /// symbol than a laptop screen can show crisply.
    private static let correctionLevel = "M"

    /// A QR code for `text`, or `nil` if CoreImage will not encode it.
    ///
    /// `nil` is a real answer, not a defensive one: the generator refuses a
    /// payload past its capacity, and a screen that drew nothing rather than
    /// saying so would be a pairing screen that silently cannot be paired
    /// with. Callers must show the payload as text instead.
    static func image(for text: String, side: CGFloat = 240) -> PlatformImage? {
        guard !text.isEmpty, side > 0 else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = correctionLevel
        guard let output = filter.outputImage else { return nil }

        let modules = output.extent.width
        guard modules > 0 else { return nil }
        let scale = max(1, (side / modules).rounded(.down))
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))

        // A context per call. `CIContext` is not `Sendable` and this runs once
        // per pairing screen, so caching one would trade a real concurrency
        // constraint for an invisible saving.
        let context = CIContext()
        guard let cgImage = context.createCGImage(scaled, from: scaled.extent) else { return nil }
        let size = CGSize(width: scaled.extent.width, height: scaled.extent.height)
        return PlatformImage.fromCGImage(cgImage, size: size)
    }
}
