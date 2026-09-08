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

// MARK: - Selectable list rows

extension View {
    /// Make a tagged `List(selection:)` row respond to a **tap**.
    ///
    /// A no-op on macOS, where clicking a tagged row already selects it. On
    /// iOS it does not: outside edit mode a tagged row is not an activatable
    /// control, so a sidebar built from `.tag(_:)` draws correctly, highlights
    /// nothing, and navigates nowhere. Every entry in `BrowseSidebar` — Today,
    /// the Inbox, every stream and every context — was inert on the phone for
    /// exactly this reason.
    ///
    /// A tap gesture rather than wrapping each row in a `NavigationLink`,
    /// because those rows carry drop destinations, context menus and
    /// `onMove`; a link around them changes what the drag system sees, and
    /// this changes nothing but what a tap does.
    @ViewBuilder
    func selectableOnTouch<Value: Hashable>(
        _ value: Value,
        selection: Binding<Value?>
    ) -> some View {
        #if os(macOS)
        self
        #else
        contentShape(.rect)
            .onTapGesture { selection.wrappedValue = value }
        #endif
    }
}

// MARK: - Naming the device

extension Platform {
    /// What to call the device the user is holding, in a sentence.
    ///
    /// The vault, the pairing flow and the lock screen all explain themselves
    /// in terms of *this device* — "there is a vault on this Mac, but its key
    /// is not in this Keychain". Those sentences were written when there was
    /// one client, and read as a bug on a phone.
    ///
    /// `UIDevice.model` rather than a hardcoded "iPhone": the same binary runs
    /// on iPad, and a pairing screen that calls an iPad an iPhone is the kind
    /// of small wrongness that makes someone distrust the much larger claim
    /// the sentence is making about their keys.
    @MainActor
    static var deviceName: String {
        #if os(macOS)
        "Mac"
        #else
        UIDevice.current.model
        #endif
    }
}

// MARK: - Checklist toggles

#if os(iOS)
/// A checklist item's tick, drawn the way iOS draws one.
///
/// macOS has `.checkbox` and iOS does not. The default iOS style is a switch,
/// which is the wrong affordance entirely inside a note: a switch says "this
/// setting is on", and a checklist item says "this thing is done". Reminders
/// and Notes both draw a circle that fills, so this does too.
struct ChecklistToggleStyle: ToggleStyle {
    func makeBody(configuration: Configuration) -> some View {
        Button {
            configuration.isOn.toggle()
        } label: {
            Image(systemName: configuration.isOn ? "checkmark.circle.fill" : "circle")
                .imageScale(.large)
                .foregroundStyle(configuration.isOn ? Color.accentColor : Color.secondary)
        }
        .buttonStyle(.plain)
        // The label is hidden at the call site, so the tick carries the name.
        .accessibilityLabel(configuration.isOn ? "Done" : "Not done")
    }
}

extension ToggleStyle where Self == ChecklistToggleStyle {
    static var checklist: ChecklistToggleStyle { ChecklistToggleStyle() }
}
#endif

// MARK: - Image bytes

extension PlatformImage {
    /// The image as PNG bytes, or `nil` if it cannot be encoded.
    ///
    /// Exists because the two platforms have no common accessor: `UIImage` has
    /// `pngData()`, and `NSImage` goes the long way round through
    /// `tiffRepresentation` and an `NSBitmapImageRep`.
    ///
    /// Used to assert that the QR renderer is deterministic — the same payload
    /// must draw the same symbol every call, or the pairing screen repaints
    /// under a camera mid-scan. That claim matters more on iOS than on macOS,
    /// since iOS is the side holding the camera.
    func pngBytes() -> Data? {
        #if os(macOS)
        guard let tiff = tiffRepresentation,
              let rep = NSBitmapImageRep(data: tiff) else { return nil }
        return rep.representation(using: .png, properties: [:])
        #else
        return pngData()
        #endif
    }
}

// MARK: - Software-keyboard input modes

/// What a text field holds, as far as a software keyboard is concerned.
///
/// `docs/08-features/keyboard.md` §Mobile keyboards requires a `text` input
/// mode with autocorrect off wherever capture syntax is typed, so that `#`,
/// `@`, `^`, `!` and `~` are read as the parser will read them and predictive
/// text cannot rewrite a token. The cases below are the field's *content*
/// rather than a platform's spelling of it, because that is the thing a call
/// site can be right or wrong about: a task title genuinely wants autocorrect,
/// and a relay URL genuinely does not.
///
/// Fields that hold ordinary prose — a task title, a routine title, a block
/// title, a review note, a stream description, a note body — get **no** case
/// here and keep the system defaults. Autocorrect on a sentence somebody
/// dictated is the feature, not the defect.
enum TextInputKind {
    /// Capture and search: the app's own syntax, typed literally.
    case syntax
    /// A lowercase name that is also a token elsewhere — a stream, a context,
    /// a saved view, a code block's language.
    case identifier
    /// An opaque block of ASCII the user pastes or reads out: a pairing
    /// message, an OIDC client id, a fenced code block.
    case opaque
    case url
    case email
    /// A count with no units in the field itself.
    case number
}

extension View {
    /// Say what this field holds, so a software keyboard can stop guessing.
    func textInput(_ kind: TextInputKind) -> some View {
        modifier(TextInputMode(kind: kind))
    }
}

/// The platform half of ``TextInputKind``.
///
/// `keyboardType` and `textInputAutocapitalization` do not exist in macOS's
/// SwiftUI at all — not as no-ops, as absent declarations — so this cannot be
/// a flat list of modifiers at the call sites. `autocorrectionDisabled` does
/// exist on both and means the same thing on both, so it is applied on both:
/// a Mac's automatic substitutions have no more business inside `#travel`
/// than a phone's.
///
/// `textContentType` is deliberately **not** here. It is an autofill hint
/// rather than an input mode, and its argument is a different type on each
/// platform (`UITextContentType` against `NSTextContentType`, whose case list
/// is much the smaller), so it belongs at the one call site that wants it.
private struct TextInputMode: ViewModifier {
    let kind: TextInputKind

    @ViewBuilder
    func body(content: Content) -> some View {
        #if os(macOS)
        switch kind {
        // Nothing to say about a count on a Mac: `autocorrectionDisabled(false)`
        // would be a decision where the field previously had none.
        case .number: content
        default: content.autocorrectionDisabled()
        }
        #else
        switch kind {
        // One arm, two cases, and both are worth keeping: they ask for the
        // same keyboard today and they are different claims about the field.
        // A stream name is a name that happens to be lowercase; a capture line
        // is a grammar. If the grammar ever wants a key row of its own, this
        // is where the two part company.
        case .syntax, .identifier:
            content
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .keyboardType(.default)
        case .opaque:
            content
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .keyboardType(.asciiCapable)
        case .url:
            content
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .keyboardType(.URL)
        case .email:
            content
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .keyboardType(.emailAddress)
        case .number:
            // `.numbersAndPunctuation` rather than `.numberPad`, which is the
            // obvious choice and the wrong one here. The number pad has no
            // return key, and these two fields sit inside a `Form` in a sheet
            // where there is nothing else to tap — so the pad puts digits one
            // tap closer and takes away the only way out. This layout leads
            // with the digits and keeps the key that dismisses it.
            content.keyboardType(.numbersAndPunctuation)
        }
        #endif
    }
}
