import CoreGraphics
import Foundation
import ImageIO
import Testing

@testable import Sunrise

/// ADR-0053 §1, the encoding half: JPEG or PNG, at most 512 px, never
/// upscaled, one chunk.
struct ThumbnailRendererTests {
    private func image(width: Int, height: Int, alpha: Bool) throws -> CGImage {
        let space = try #require(CGColorSpace(name: CGColorSpace.sRGB))
        let context = try #require(CGContext(
            data: nil,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: 0,
            space: space,
            bitmapInfo: alpha
                ? CGImageAlphaInfo.premultipliedLast.rawValue
                : CGImageAlphaInfo.noneSkipLast.rawValue
        ))
        // Noise, so the encoder has something to spend bytes on.
        var seed: UInt32 = 7
        for row in stride(from: 0, to: height, by: 4) {
            for column in stride(from: 0, to: width, by: 4) {
                seed = seed &* 1_664_525 &+ 1_013_904_223
                let shade = CGFloat(seed % 255) / 255
                context.setFillColor(red: shade, green: 1 - shade, blue: 0.5, alpha: alpha ? 0.5 : 1)
                context.fill(CGRect(x: column, y: row, width: 4, height: 4))
            }
        }
        return try #require(context.makeImage())
    }

    private func decoded(_ data: Data) throws -> CGImage {
        let source = try #require(CGImageSourceCreateWithData(data as CFData, nil))
        return try #require(CGImageSourceCreateImageAtIndex(source, 0, nil))
    }

    @Test
    func anOpaqueImageBecomesAJpegNoLargerThan512() throws {
        let out = try #require(ThumbnailRenderer.encode(try image(width: 2000, height: 1000, alpha: false)))
        #expect(out.mime == "image/jpeg")
        #expect(out.data.count <= ThumbnailRenderer.maxBytes)
        #expect(out.data.starts(with: [0xFF, 0xD8, 0xFF]))
        let back = try decoded(out.data)
        #expect(back.width == 512 && back.height == 256)
    }

    @Test
    func anImageWithAlphaBecomesAPng() throws {
        let out = try #require(ThumbnailRenderer.encode(try image(width: 600, height: 600, alpha: true)))
        #expect(out.mime == "image/png")
        #expect(out.data.count <= ThumbnailRenderer.maxBytes)
        #expect(out.data.starts(with: [0x89, 0x50, 0x4E, 0x47]))
    }

    @Test
    func aSmallImageIsNotUpscaled() throws {
        let out = try #require(ThumbnailRenderer.encode(try image(width: 120, height: 80, alpha: false)))
        let back = try decoded(out.data)
        #expect(back.width == 120 && back.height == 80)
    }

    /// Redrawn into a fresh bitmap, so nothing of the original's metadata
    /// survives: no EXIF and no GPS.
    @Test
    func theThumbnailCarriesNoMetadata() throws {
        let out = try #require(ThumbnailRenderer.encode(try image(width: 300, height: 300, alpha: false)))
        let source = try #require(CGImageSourceCreateWithData(out.data as CFData, nil))
        let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any] ?? [:]
        #expect(props[kCGImagePropertyGPSDictionary] == nil)
        #expect(props[kCGImagePropertyTIFFDictionary] == nil)
    }
}

/// Settings → Storage against a real vault.
@MainActor
struct AttachmentCacheModelTests {
    @Test
    func theCellularToggleAndTheLimitAreDevicePreferences() async throws {
        let vault = try await TestVault()
        let model = AttachmentCacheModel(bridge: vault.bridge)
        await model.refresh()
        #expect(model.usedBytes == 0)
        #expect(model.limitBytes >= 200_000_000, "a default from the key table")
        #expect(!model.autoFetchOnCellular, "off by default")

        await model.setAutoFetchOnCellular(true)
        #expect(model.autoFetchOnCellular)
        await model.setLimit(500_000_000)
        #expect(model.limitBytes == 500_000_000)
        #expect(model.errorMessage == nil)

        await model.clear()
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }
}
