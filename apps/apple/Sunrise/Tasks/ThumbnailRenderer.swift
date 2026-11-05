import CoreGraphics
import Foundation
import ImageIO
import QuickLookThumbnailing
import UniformTypeIdentifiers

/// The source device's half of ADR-0053 §1: render a thumbnail of a file
/// being attached, with the platform's own APIs, in the one shape every
/// receiver can decode.
///
/// - **What:** whatever QuickLook Thumbnailing can draw — images, the first
///   page of a PDF, a video's poster frame, documents the OS has a
///   thumbnailer for.
/// - **Format:** JPEG at quality 0.8 when the image has no alpha, PNG when it
///   has. Never AVIF, HEIC or WebP.
/// - **Size:** longest edge at most 512 px, never upscaled, drawn into sRGB.
/// - **Metadata:** none of the original's. The image is redrawn into a fresh
///   bitmap before it is encoded, so no EXIF, GPS or XMP of the original
///   reaches the thumbnail. What ImageIO's JPEG encoder writes of its own is
///   three Exif tags about the thumbnail itself: sRGB and its pixel size.
/// - **One chunk:** at most 256 KiB. A JPEG over it is re-encoded at 0.6, then
///   0.4; a PNG is redrawn at 384 px, then 256 px; and a thumbnail that still
///   does not fit is not made.
///
/// The core checks the type and the size again before it seals anything, so
/// a renderer bug costs a thumbnail, never a malformed blob.
enum ThumbnailRenderer {
    /// One blob chunk, the most a thumbnail may be.
    static let maxBytes = 256 * 1024

    /// What a thumbnail and the original's dimensions come to, ready for the
    /// core.
    static func preview(of url: URL) async -> AttachPreviewIn {
        let size = pixelSize(of: url)
        let thumbnail = await render(url)
        return AttachPreviewIn(
            width: size?.width,
            height: size?.height,
            thumbnailMime: thumbnail?.mime,
            thumbnailBytes: thumbnail?.data
        )
    }

    /// The pixel size of an image file, orientation applied, or `nil` for a
    /// file ImageIO cannot read as an image.
    static func pixelSize(of url: URL) -> (width: UInt32, height: UInt32)? {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil)
                as? [CFString: Any],
              let width = props[kCGImagePropertyPixelWidth] as? Int,
              let height = props[kCGImagePropertyPixelHeight] as? Int,
              width > 0, height > 0
        else { return nil }
        // EXIF orientations 5 to 8 are the four that rotate by a quarter turn.
        let orientation = props[kCGImagePropertyOrientation] as? UInt32 ?? 1
        let wide = UInt32(clamping: width)
        let high = UInt32(clamping: height)
        return (5...8).contains(orientation) ? (high, wide) : (wide, high)
    }

    /// Ask QuickLook for the file's thumbnail and encode it, or `nil` when
    /// the OS has none for this type or it cannot be made small enough.
    static func render(_ url: URL) async -> (mime: String, data: Data)? {
        let request = QLThumbnailGenerator.Request(
            fileAt: url,
            size: CGSize(width: 512, height: 512),
            scale: 1,
            representationTypes: .thumbnail
        )
        guard let representation = try? await QLThumbnailGenerator.shared
            .generateBestRepresentation(for: request)
        else { return nil }
        return encode(representation.cgImage)
    }

    /// The format and size ladder, on an image already in hand.
    static func encode(_ image: CGImage) -> (mime: String, data: Data)? {
        if hasAlpha(image) {
            for edge in [512, 384, 256] {
                if let drawn = redraw(image, longestEdge: CGFloat(edge), alpha: true),
                   let data = encoded(drawn, type: .png, quality: nil),
                   data.count <= maxBytes {
                    return ("image/png", data)
                }
            }
            return nil
        }
        guard let drawn = redraw(image, longestEdge: 512, alpha: false) else { return nil }
        for quality in [0.8, 0.6, 0.4] {
            if let data = encoded(drawn, type: .jpeg, quality: quality), data.count <= maxBytes {
                return ("image/jpeg", data)
            }
        }
        return nil
    }

    static func hasAlpha(_ image: CGImage) -> Bool {
        switch image.alphaInfo {
        case .none, .noneSkipFirst, .noneSkipLast: false
        default: true
        }
    }

    /// `image` drawn into a fresh sRGB bitmap whose longest edge is at most
    /// `longestEdge`, never larger than the source.
    static func redraw(_ image: CGImage, longestEdge: CGFloat, alpha: Bool) -> CGImage? {
        let longest = CGFloat(max(image.width, image.height))
        guard longest > 0 else { return nil }
        let scale = min(1, longestEdge / longest)
        let width = max(1, Int((CGFloat(image.width) * scale).rounded()))
        let height = max(1, Int((CGFloat(image.height) * scale).rounded()))
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let context = CGContext(
                  data: nil,
                  width: width,
                  height: height,
                  bitsPerComponent: 8,
                  bytesPerRow: 0,
                  space: space,
                  bitmapInfo: alpha
                      ? CGImageAlphaInfo.premultipliedLast.rawValue
                      : CGImageAlphaInfo.noneSkipLast.rawValue
              )
        else { return nil }
        context.interpolationQuality = .high
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage()
    }

    static func encoded(_ image: CGImage, type: UTType, quality: Double?) -> Data? {
        let data = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(
            data as CFMutableData,
            type.identifier as CFString,
            1,
            nil
        ) else { return nil }
        var options: [CFString: Any] = [:]
        if let quality { options[kCGImageDestinationLossyCompressionQuality] = quality }
        CGImageDestinationAddImage(destination, image, options as CFDictionary)
        guard CGImageDestinationFinalize(destination) else { return nil }
        return data as Data
    }
}
