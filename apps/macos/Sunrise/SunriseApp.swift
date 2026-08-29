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
            // The menu bar is where `docs/08-features/keyboard.md`'s
            // application-scope bindings live. Not decoration: the
            // accessibility spec forbids a shortcut with no visible
            // affordance, and a menu item is the affordance macOS already has
            // — it works with no window open, it is readable by VoiceOver, and
            // the system's own Keyboard settings can rebind it.
            CommandGroup(replacing: .newItem) {
                NewMenuItems(surfaces: surfaces)
            }
            CommandGroup(after: .appInfo) {
                AppMenuItems(surfaces: surfaces)
            }
            CommandGroup(after: .toolbar) {
                GoMenuItems(surfaces: surfaces)
            }
            CommandGroup(after: .help) {
                Divider()
                CommandMenuItem(surfaces: surfaces, action: .cheatSheet)
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

/// What the app menu adds: capture, and the two daily briefs.
///
/// A `View` rather than the buttons written inline, so that it has an
/// environment to read `openWindow` from — the menu has to work with every
/// window closed, which is exactly when someone reaches for ⌘⌥M.
private struct AppMenuItems: View {
    let surfaces: AppSurfaces

    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Quick Capture") { surfaces.openQuickCapture() }
            .keyboardShortcut("n", modifiers: [.command, .shift])
        Divider()
        // The same two destinations a reminder opens into. A view reachable
        // only from a notification would be a view nobody who declined
        // notifications ever sees.
        Button("Morning Summary") { show(.morningSummary) }
            .keyboardShortcut("m", modifiers: [.command, .option])
        Button("End of Day") { show(.endOfDayPlan) }
            .keyboardShortcut("e", modifiers: [.command, .option])
    }

    private func show(_ link: DeepLink) {
        openWindow(id: SunriseWindow.main.rawValue)
        surfaces.open(link)
    }
}

/// One menu item, bound to whatever the keymap says.
///
/// A view rather than a `Button` written out ten times, because every one of
/// them does the same two things: open a window if there is none, and hand the
/// action to it. ⌘1 pressed with the app in the background has to work, and
/// that is the half people forget.
private struct CommandMenuItem: View {
    let surfaces: AppSurfaces
    let action: AppAction

    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button(action.title) {
            openWindow(id: SunriseWindow.main.rawValue)
            surfaces.request(action)
        }
        .keyboardShortcut(for: action)
    }
}

/// The File menu's top: capture, and a new stream.
private struct NewMenuItems: View {
    let surfaces: AppSurfaces

    var body: some View {
        CommandMenuItem(surfaces: surfaces, action: .quickCapture)
        CommandMenuItem(surfaces: surfaces, action: .newStream)
    }
}

/// The View menu's additions: the two fixed lists, both searches, and the
/// palette.
private struct GoMenuItems: View {
    let surfaces: AppSurfaces

    var body: some View {
        Divider()
        CommandMenuItem(surfaces: surfaces, action: .today)
        CommandMenuItem(surfaces: surfaces, action: .inbox)
        Divider()
        CommandMenuItem(surfaces: surfaces, action: .searchInView)
        CommandMenuItem(surfaces: surfaces, action: .searchGlobal)
        Divider()
        CommandMenuItem(surfaces: surfaces, action: .commandPalette)
    }
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

    /// The vault these surfaces are bound to, or `nil` between one being
    /// closed and the next being opened.
    ///
    /// Held rather than only closed over, because a capture has to be written
    /// to whichever core is open *at the moment return is pressed*. A closure
    /// that captured the bridge `attach` was called with would keep writing
    /// into a `Core` that ``SessionModel/switchTo(_:)`` has already shut down.
    private(set) var vault: CoreBridge?

    /// The local-notification schedule. Absent until the vault opens, for the
    /// same reason the menu bar is: there is nothing to remind anyone about
    /// until there is something to read it from.
    private(set) var reminders: ReminderScheduler?

    /// Where a deep link, a notification tap or ⌘⌥M wants the window to be.
    ///
    /// Set here and consumed by `VaultView`, because the window's selection is
    /// the window's state: this object exists in scenes that have no sidebar
    /// at all, and reaching into one from here would be reaching into a view
    /// that may not be on screen.
    private(set) var pendingDestination: Destination?

    /// What a menu item asked the window to do.
    ///
    /// The same shape as ``pendingDestination`` and for the same reason: the
    /// menu bar exists in scenes that have no list, no palette and no capture
    /// field, so a menu item cannot reach into one. It leaves the request here
    /// and the window picks it up.
    private(set) var pendingCommand: AppAction?

    /// Notification settings are per device and never sync, so they live
    /// beside the vault rather than in it — and they are read before the vault
    /// is open, which is why this is not created in `attach`.
    let notifications = NotificationPreferences()

    private let hotkey = HotkeyCenter()
    private var panel: QuickCapturePanel?
    private var capture: CaptureModel?
    private var hotkeyWatcher: _Concurrency.Task<Void, Never>?

    /// Bind to an open vault. Called when the session unlocks, and again on
    /// every vault it opens after that.
    ///
    /// **Rebuilds rather than skipping.** This used to return early when
    /// `menuBar` was already set, which was right while a Mac had exactly one
    /// vault and wrong the moment multi-account switching landed:
    /// ``SessionModel/switchTo(_:)`` shuts the old `Core` down, and every
    /// object below holds a bridge to it. Left in place they would show the
    /// previous vault's counts, schedule the previous vault's reminders, and —
    /// worst — accept a ⌘⇧N capture into a core that cannot write it.
    ///
    /// Still safe to call twice with the same bridge: tearing down and
    /// rebuilding costs one subscription, and the alternative is the silent
    /// data loss above.
    func attach(bridge: CoreBridge) {
        releaseVault()
        vault = bridge
        menuBar = MenuBarModel(bridge: bridge)
        let panel = QuickCapturePanel(bridge: bridge) { [weak self] draft in
            try await self?.commitCapture(draft)
        }
        self.panel = panel
        capture = panel.capture
        reminders = ReminderScheduler(
            bridge: bridge,
            preferences: notifications
        ) { [weak self] link in
            // A tapped notification arrives outside every view, so it is the
            // one route that may have no window to land in.
            self?.open(link, raisingAWindow: true)
        }
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
    func openQuickCapture(prefill: String = "") {
        guard let panel else {
            NSSound.beep()
            return
        }
        panel.present()
        if !prefill.isEmpty { capture?.text = prefill }
    }

    /// Write one captured line into the vault that is open *now*.
    ///
    /// Throws rather than swallowing. This used to be `try?` inside the panel,
    /// which meant a capture the core refused — most sharply, one aimed at a
    /// vault that had just been switched away from — vanished with no error at
    /// all. Quick capture is the fastest way in this app to record a thought
    /// and it was, until this, also the fastest way to lose one.
    func commitCapture(_ draft: TaskDraftIn) async throws {
        guard let vault else { throw CaptureError.noOpenVault }
        _ = try await vault.submit(.createTask(draft: draft))
    }

    /// Act on a `sunrise://` link, from wherever it came.
    ///
    /// One entry point for all three sources — an external URL, a tapped
    /// notification, and the app menu — so a link cannot behave differently
    /// depending on who opened it. What each one does is
    /// ``DeepLink/destination``'s decision, not this method's; the split
    /// between "goes to a screen" and "runs and stays out of the way" is the
    /// interesting part and it is stated once, where it can be tested.
    func open(_ link: DeepLink, raisingAWindow: Bool = false) {
        if case let .capture(text) = link {
            openQuickCapture(prefill: text)
            return
        }
        if case let .task(entity, .complete) = link {
            perform(.complete, on: entity)
        }
        if case let .task(entity, .snooze(span)) = link {
            perform(.snooze(span), on: entity)
        }
        guard let destination = link.destination else { return }
        pendingDestination = destination
        if raisingAWindow { raiseWindow(for: link) }
    }

    /// Make sure something is on screen to receive ``pendingDestination``.
    ///
    /// SwiftUI can only open a `WindowGroup` window from inside a view —
    /// `openWindow` is an `@Environment` value — and a notification tap
    /// arrives outside every one. Handing the link back to LaunchServices is
    /// the supported way round it: the OS delivers it to this app's
    /// `WindowGroup`, which opens a window to receive it, and the `onOpenURL`
    /// on `RootView` finds the destination already set.
    ///
    /// Not recursive, by construction: `onOpenURL` calls ``open(_:)`` without
    /// this flag, and the check below short-circuits once a window exists
    /// anyway.
    private func raiseWindow(for link: DeepLink) {
        let app = NSApplication.shared
        app.activate(ignoringOtherApps: true)
        // Panels do not count: the capture field and the menu bar's own window
        // cannot show a screen.
        let hasWindow = app.windows.contains { $0.isVisible && $0.canBecomeMain }
        guard !hasWindow, let url = link.url else { return }
        NSWorkspace.shared.open(url)
    }

    /// The window has taken the pending destination; stop offering it.
    ///
    /// Cleared rather than left set, so that navigating away from the morning
    /// view and pressing ⌘⌥M again goes back to it instead of doing nothing —
    /// the same value twice is two requests, not one.
    func destinationTaken() {
        pendingDestination = nil
    }

    /// Ask the window to run a keyboard action.
    ///
    /// Cleared by ``commandTaken()`` the moment the window has it, so pressing
    /// ⌘⇧P twice opens the palette twice — the same value set twice with no
    /// clearing in between is one change, and one change is one palette.
    func request(_ action: AppAction) {
        pendingCommand = action
    }

    func commandTaken() {
        pendingCommand = nil
    }

    private func perform(_ action: ReminderAction, on entity: EntityRef) {
        guard let reminders else {
            NSSound.beep()
            return
        }
        _Concurrency.Task { await reminders.perform(action, on: entity) }
    }

    /// Let go of everything bound to the vault that is open.
    ///
    /// Not a `deinit`: `hotkeyWatcher` is main-actor isolated and a `deinit` is
    /// not, so cancelling there is exactly the kind of cross-actor touch Swift
    /// 6 refuses. `attach` calls this first, and a test calls ``detach()``.
    ///
    /// The panel is closed rather than merely dropped, because its content view
    /// is an `NSHostingView` holding the old `CaptureModel` — and a borderless
    /// panel left on screen over the new vault would take a line and write it
    /// nowhere.
    private func releaseVault() {
        hotkeyWatcher?.cancel()
        hotkeyWatcher = nil
        panel?.close()
        panel = nil
        capture = nil
        menuBar = nil
        reminders = nil
        vault = nil
    }

    /// Release the vault surfaces *and* the hotkey.
    ///
    /// The hotkey is the one thing `attach` does not rebuild — it is a
    /// registration with the window server, not a thing bound to a vault — so
    /// giving it back belongs here and not in ``releaseVault()``.
    func detach() {
        releaseVault()
        hotkey.unregister()
        hotkeyStatus = hotkey.status
    }
}

/// Why a capture could not be written.
enum CaptureError: Error, Equatable, LocalizedError {
    /// No vault is open — the window between one being closed and the next
    /// being opened, which multi-account switching made reachable.
    case noOpenVault

    var errorDescription: String? {
        switch self {
        case .noOpenVault:
            "No vault is open. Open Sunrise and try again."
        }
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
            // Keyed on the model's identity, not left bare. A vault switch
            // hands this branch a *different* `MenuBarModel` in the same place
            // in the view tree, and a bare `.task` would keep running against
            // the one that was replaced — the new vault's counts would never
            // be read.
            .task(id: ObjectIdentifier(model)) { await model.refresh() }
            .task(id: ObjectIdentifier(model)) { await model.poll() }
            .task(id: ObjectIdentifier(model)) { await model.follow() }
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
