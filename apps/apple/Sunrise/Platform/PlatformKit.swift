#if os(macOS)
import AppKit
#else
import UIKit
#endif
import SwiftUI

/// The handful of places two Apple platforms spell the same idea differently.
///
/// Deliberately small, and deliberately not an abstraction layer. Everything
/// here is a rename of something both platforms have: an image, the clipboard,
/// "open this URL somewhere else", and the noise a refusal makes. A shim that
/// grew past that would become a third UI framework to maintain, which is the
/// opposite of what sharing the sources is for.
///
/// What is *not* here is as load-bearing as what is. A menu bar, a global
/// hotkey, a borderless panel and an `NSOpenPanel` have no iOS spelling at
/// all — they are surfaces one platform has and the other does not, so they
/// live in `macOS/` and their iOS counterparts live in `iOS/`, rather than
/// being papered over with a protocol that would have one real conformance.
enum Platform {}

// MARK: - Images

#if os(macOS)
/// `NSImage` on macOS, `UIImage` on iOS.
///
/// Both are what `Image(nsImage:)` / `Image(uiImage:)` take, and both are what
/// `CGImage`-producing code (`QRCode`, attachment previews) has to hand back.
typealias PlatformImage = NSImage
#else
typealias PlatformImage = UIImage
#endif

extension PlatformImage {
    /// Wrap a `CGImage` at a given point size.
    ///
    /// The two initialisers disagree in a way worth hiding: `NSImage` takes an
    /// explicit size in points, and `UIImage` takes a scale factor and derives
    /// the size from it. Callers want "this many points square", so that is
    /// what this takes.
    static func fromCGImage(_ image: CGImage, size: CGSize) -> PlatformImage {
        #if os(macOS)
        NSImage(cgImage: image, size: size)
        #else
        // `scale` is what turns pixels into points for UIKit. Deriving it from
        // the requested width keeps the result the size the caller asked for
        // on any device, rather than the size the bitmap happens to be.
        let scale = size.width > 0 ? CGFloat(image.width) / size.width : 1
        return UIImage(cgImage: image, scale: max(scale, 1), orientation: .up)
        #endif
    }
}

extension Image {
    /// `Image(nsImage:)` / `Image(uiImage:)` under one name.
    init(platformImage: PlatformImage) {
        #if os(macOS)
        self.init(nsImage: platformImage)
        #else
        self.init(uiImage: platformImage)
        #endif
    }
}

// MARK: - The clipboard

/// Read and write the system clipboard.
///
/// A type rather than free functions so a test can hold one; the pasting leg
/// of pairing is the reason this exists, and it is the one clipboard use in
/// the app whose failure a user would actually notice.
enum PlatformPasteboard {
    /// The plain-text contents, or `nil` when the clipboard holds none.
    static var string: String? {
        #if os(macOS)
        NSPasteboard.general.string(forType: .string)
        #else
        UIPasteboard.general.string
        #endif
    }

    /// Replace the clipboard's contents with `text`.
    static func set(_ text: String) {
        #if os(macOS)
        // AppKit requires the clear before the write; skipping it appends a
        // second representation rather than replacing the first.
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        #else
        UIPasteboard.general.string = text
        #endif
    }
}

// MARK: - Leaving the app

extension Platform {
    /// Hand a URL to whatever the system thinks should handle it.
    ///
    /// The OIDC sign-in leg and the two "read the documentation" links. On
    /// iOS this is asynchronous and can report a refusal, which nothing here
    /// currently reads — the caller's fallback in both cases is the same
    /// as macOS's, which is that nothing visibly happens.
    @MainActor
    static func openExternal(_ url: URL) {
        #if os(macOS)
        NSWorkspace.shared.open(url)
        #else
        UIApplication.shared.open(url)
        #endif
    }

    /// The noise a refusal makes.
    ///
    /// macOS beeps. iOS has no system beep for this — an app that made a sound
    /// when a button did nothing would be reporting a bug as an alarm — so it
    /// plays the error haptic instead, which is what an iOS refusal feels
    /// like. Both are a last resort behind a real explanation, never instead
    /// of one.
    @MainActor
    static func refusalFeedback() {
        #if os(macOS)
        NSSound.beep()
        #else
        UINotificationFeedbackGenerator().notificationOccurred(.error)
        #endif
    }
}
