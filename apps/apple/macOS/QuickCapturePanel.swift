import AppKit
import SwiftUI

/// The borderless capture window.
///
/// An `NSPanel` rather than a SwiftUI `Window` scene, and the reason is the
/// hotkey. A SwiftUI window is opened through `@Environment(\.openWindow)`,
/// which only exists inside a view — so the hotkey could only reach it while
/// some other window happened to be on screen. A panel is owned by the object
/// that owns the hotkey, and works with every window closed, which is exactly
/// when someone reaches for a capture shortcut.
///
/// `.nonactivatingPanel` matters too: capture must not pull the whole app
/// forward and push aside what the user was reading when they had the thought.
@MainActor
final class QuickCapturePanel {
    private let panel: NSPanel
    /// Exposed so a `sunrise://capture?text=…` link can seed the field after
    /// `present()` has cleared it.
    let capture: CaptureModel

    init(bridge: CoreBridge, commit: @escaping (TaskDraftIn) async throws -> Void) {
        capture = CaptureModel(bridge: bridge)
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 120),
            styleMask: [.titled, .fullSizeContentView, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        let model = capture
        let dismiss: () -> Void = { [weak panel] in panel?.orderOut(nil) }
        panel.contentView = NSHostingView(
            rootView: QuickCaptureView(model: model, commit: commit, dismiss: dismiss)
        )
        FloatingPanel.configure(panel)
        panel.isReleasedWhenClosed = false
    }

    /// Bring it forward, cleared, and give it the keyboard.
    ///
    /// Cleared on purpose: a capture field still holding last night's
    /// half-typed line is a field that commits the wrong thing the first time
    /// someone hits return without reading it.
    func present() {
        capture.clear()
        panel.center()
        NSApplication.shared.activate(ignoringOtherApps: true)
        panel.makeKeyAndOrderFront(nil)
    }

    /// Take it off screen and drop what it was hosting.
    ///
    /// `isReleasedWhenClosed` is false, so `close()` is not a deallocation —
    /// clearing `contentView` is what actually lets go of the `CaptureModel`
    /// and the bridge behind it.
    func close() {
        panel.orderOut(nil)
        panel.contentView = nil
        panel.close()
    }
}
