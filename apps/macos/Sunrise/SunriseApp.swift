import AppKit
import SwiftUI

/// The macOS client.
///
/// One main window, no document model: the vault is the document and there is
/// exactly one of it per machine. ADR-0019 records why this is a native
/// SwiftUI app rather than the Tauri desktop shell the docs used to specify.
///
/// The other two surfaces exist because a vault is a *daily* thing rather than
/// an app you open. Quick capture is a borderless panel on a system-wide
/// hotkey, and the menu bar item is the glance that says whether opening the
/// main window is worth it.
@main
struct SunriseApp: App {
    /// The session is owned here rather than by `RootView` so that all three
    /// surfaces share one open vault. The core holds the vault lock for as long
    /// as it runs and a second `Core::open` on the same directory is refused —
    /// so a menu bar item with a vault of its own would not merely be
    /// wasteful, it would fail to start.
    @State private var session = SessionModel.standard()
    @State private var surfaces = AppSurfaces()

    var body: some Scene {
        WindowGroup("Sunrise", id: SunriseWindow.main.rawValue) {
            RootView(session: session, surfaces: surfaces)
                .frame(minWidth: 720, minHeight: 480)
        }
        .defaultSize(width: 1000, height: 700)
        .commands {
            CommandGroup(replacing: .newItem) {}
            CommandGroup(after: .appInfo) {
                Button("Quick Capture") { surfaces.openQuickCapture() }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
            }
        }

        MenuBarExtra {
            MenuBarScene(surfaces: surfaces)
        } label: {
            MenuBarLabel(surfaces: surfaces)
        }
        .menuBarExtraStyle(.window)
    }
}

/// The app's windows, by id.
enum SunriseWindow: String {
    case main
}

/// Cross-surface state: the menu bar's snapshot, the hotkey, and the quick
/// capture panel.
///
/// One object rather than a model per surface. `MenuBarExtra` and the main
/// window are separate scenes with separate view hierarchies, and two
/// `MenuBarModel`s would mean two change subscriptions and two sets of counts
/// that could disagree on screen at the same time.
@MainActor
@Observable
final class AppSurfaces {
    private(set) var menuBar: MenuBarModel?
    private(set) var hotkeyStatus: HotkeyStatus = .idle

    private let hotkey = HotkeyCenter()
    private var panel: QuickCapturePanel?
    private var hotkeyWatcher: _Concurrency.Task<Void, Never>?

    /// Bind to an open vault. Called when the session unlocks; idempotent, so
    /// a re-render cannot double the subscriptions behind it.
    func attach(bridge: CoreBridge) {
        guard menuBar == nil else { return }
        menuBar = MenuBarModel(bridge: bridge)
        panel = QuickCapturePanel(bridge: bridge)
        hotkey.register()
        hotkeyStatus = hotkey.status
        // Registration can fail — another app may already hold ⌘⇧N — and when
        // it does, nothing will ever post this. Watching anyway costs one
        // suspended task and means the menu bar and the panel take exactly the
        // same path however capture was asked for.
        hotkeyWatcher = _Concurrency.Task { @MainActor [weak self] in
            for await _ in NotificationCenter.default.notifications(named: .sunriseQuickCapture) {
                self?.openQuickCapture()
            }
        }
    }

    /// Show the capture panel, over whatever the user was doing.
    ///
    /// A no-op with an explanation rather than a crash when the vault is not
    /// open: capture writes, and a field that silently discarded what someone
    /// typed into it is worse than one that never appeared.
    func openQuickCapture() {
        guard let panel else {
            NSSound.beep()
            return
        }
        panel.present()
    }

    /// Release the hotkey watcher.
    ///
    /// Not a `deinit`: `hotkeyWatcher` is main-actor isolated and a `deinit` is
    /// not, so cancelling there is exactly the kind of cross-actor touch Swift
    /// 6 refuses. This object lives as long as the app anyway; the method
    /// exists so a test can stop the watcher it started.
    func detach() {
        hotkeyWatcher?.cancel()
        hotkeyWatcher = nil
        hotkey.unregister()
        hotkeyStatus = hotkey.status
    }
}

/// The menu bar's own label: the app's mark, plus the one number worth a
/// glance.
private struct MenuBarLabel: View {
    let surfaces: AppSurfaces

    var body: some View {
        let badge = surfaces.menuBar?.snapshot.badge ?? ""
        HStack(spacing: 3) {
            Image(systemName: "sun.max")
            if !badge.isEmpty { Text(badge) }
        }
    }
}

/// The menu bar's contents, once there is a vault behind them.
private struct MenuBarScene: View {
    let surfaces: AppSurfaces

    @Environment(\.openWindow) private var openWindow

    var body: some View {
        if let model = surfaces.menuBar {
            MenuBarView(
                model: model,
                openMain: { openWindow(id: SunriseWindow.main.rawValue) },
                openCapture: { surfaces.openQuickCapture() },
                hotkey: surfaces.hotkeyStatus
            )
            .task { await model.refresh() }
            .task { await model.poll() }
            .task { await model.follow() }
        } else {
            // The vault is not open: locked, first run, or still starting.
            // Saying so beats an empty menu, and the main window is where every
            // one of those is resolved.
            VStack(alignment: .leading, spacing: 8) {
                Text("Sunrise is not unlocked.").font(.callout)
                Button("Open Sunrise") { openWindow(id: SunriseWindow.main.rawValue) }
                Button("Quit Sunrise") { NSApplication.shared.terminate(nil) }
            }
            .padding(12)
            .frame(width: 220)
        }
    }
}

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
private final class QuickCapturePanel {
    private let panel: NSPanel
    private let capture: CaptureModel

    init(bridge: CoreBridge) {
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
            rootView: QuickCaptureView(
                model: model,
                commit: { draft in
                    _ = try? await bridge.submit(.createTask(draft: draft))
                },
                dismiss: dismiss
            )
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
}
