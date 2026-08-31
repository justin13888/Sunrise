import AppKit
import SwiftUI

/// Turns the SwiftUI quick-capture window into a floating borderless panel.
///
/// SwiftUI has no scene modifier for "no title bar, floats over full-screen
/// apps, and does not steal the whole app's activation". `NSWindow` does, and
/// this reaches it once, when the window appears.
///
/// There is no iOS counterpart, and that is not an omission. A phone has one
/// foreground app and no window server to float above it; the iOS capture
/// surface is a sheet over the app, plus the Control Center control and the
/// widget that open it from outside — see `docs/07-clients/parity-matrix.md`
/// §Capture-surface portability, which asks for parity of capture *semantics*
/// rather than of surfaces.
struct FloatingPanel: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { Self.configure(view.window) }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {}

    static func configure(_ window: NSWindow?) {
        guard let window else { return }
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.standardWindowButton(.closeButton)?.isHidden = true
        window.standardWindowButton(.miniaturizeButton)?.isHidden = true
        window.standardWindowButton(.zoomButton)?.isHidden = true
        window.isMovableByWindowBackground = true
        window.backgroundColor = .clear
        window.isOpaque = false
        window.hasShadow = true
        window.level = .floating
        // Follows the user to whichever Space they are on, and appears over a
        // full-screen app — which is the only way a capture window is any use
        // to someone who was in the middle of something.
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.center()
    }
}
