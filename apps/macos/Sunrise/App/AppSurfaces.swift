import AppKit
import SwiftUI

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
    /// How often the core re-materializes routines.
    ///
    /// Fifteen minutes, not a minute and not an hour. Materialization is a
    /// horizon check over the live routines — cheap, but not free — and the
    /// thing it has to beat is a reminder's lead time, which
    /// `docs/08-features/notifications.md` defaults to zero and lets a user
    /// set in minutes. An occurrence that appears a quarter of an hour late is
    /// the worst this can do; `Core::open` covers the gap at unlock, and the
    /// Routines screen's "Generate now" covers the impatient.
    static let routineIntervalMs: UInt64 = 15 * 60 * 1000

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

    /// The iCalendar import/export surface. Owned here rather than by the
    /// window because the File menu is a *scene* command: it exists with every
    /// window closed, and it has to reach whichever vault is open now.
    private(set) var ical: IcalModel?

    /// Whether the core's periodic routine materialization is running, and for
    /// which vault.
    ///
    /// Recorded rather than fired and forgotten. `Core::start_routine_timer`
    /// is idempotent and silent — it returns `Ok` when a timer is already
    /// running — so "did anything ever start one, and was it this vault's
    /// core?" is a question nothing else in the process can answer. It is the
    /// question that matters: the timer belongs to the `Core`, a vault switch
    /// shuts that `Core` down, and a client that started the timer once at
    /// launch would leave every vault opened after the first with no
    /// recurrence at all.
    private(set) var routineTimer: RoutineTimerState = .stopped

    /// Why File → Print… / Export as PDF… is unavailable for the screen the
    /// window is showing, or `nil` when it is available.
    ///
    /// Pushed here by `VaultView` rather than read from it, and for the same
    /// reason ``pendingDestination`` travels the other way: the File menu is a
    /// *scene* command that exists with every window closed, and the selection
    /// it depends on is the window's own `@State`, which nothing outside that
    /// view can observe. It starts as a refusal, which is the truthful answer
    /// while there is no window and nothing selected.
    ///
    /// The sentence rather than a `Bool` — see
    /// ``PrintDocument/refusal(for:reviewTab:)``, which produces it.
    private(set) var printRefusal: String? = "Nothing is selected."

    /// The window reporting what its current screen can do with ⌘P.
    func printRefusalChanged(to reason: String?) {
        printRefusal = reason
    }

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
        // Hand the live vault to the App Intents surface. Without this an
        // intent fired while Sunrise is open finds the one-vault-per-process
        // slot taken and refuses — correct, but a refusal where the answer
        // was available.
        IntentVault.adopt(bridge)
        menuBar = MenuBarModel(bridge: bridge)
        ical = IcalModel(bridge: bridge)
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

    /// Start the core's periodic routine materialization against the vault
    /// that is open now.
    ///
    /// Separate from ``attach(bridge:)`` only because it has to `await` — the
    /// bridge is an actor — and `attach` is called from places that cannot.
    /// Every caller of one calls the other.
    ///
    /// `docs/03-features/routines.md` asks for occurrences to appear without
    /// anybody opening the Routines screen, and until this had a caller
    /// nothing in the running app started the timer: recurrence only advanced
    /// when `Core::open` materialized once at unlock, or when somebody pressed
    /// "Generate now". A Mac left open across midnight showed yesterday's
    /// routines and no more.
    func startRoutineTimer(everyMs: UInt64 = AppSurfaces.routineIntervalMs) async {
        guard let vault else {
            routineTimer = .stopped
            return
        }
        do {
            try await vault.startRoutineTimer(everyMs: everyMs)
            routineTimer = .running(vault: ObjectIdentifier(vault))
        } catch {
            // Reported rather than thrown. Nothing on screen depends on the
            // timer having started, and an alert at unlock over a background
            // task is the wrong trade — but a silent failure would leave the
            // same "routines stopped generating" bug this method exists to
            // fix, with nothing to read.
            routineTimer = .failed(error.localizedDescription)
        }
    }

    /// **File → Import Calendar…**: pick an `.ics` and read it in.
    ///
    /// The picking and the reading are the app's; the parsing is the core's.
    /// A read that fails lands on the same `errorMessage` a refused parse does,
    /// because from where the user is standing "that file could not be read"
    /// is one outcome however far down it failed.
    func importIcal() async {
        guard let ical else {
            NSSound.beep()
            return
        }
        guard let url = IcalFiles.pickDocument() else { return }
        do {
            await ical.importDocument(text: try IcalFiles.read(url))
        } catch {
            ical.summary = nil
            ical.errorMessage = error.localizedDescription
        }
    }

    /// **File → Export Calendar ▸ …**: render a window and write it out.
    ///
    /// Destination first, then render, which is the order every macOS save
    /// takes — and it means a cancelled panel costs nothing.
    func exportIcal(_ window: ExportWindow) async {
        guard let ical else {
            NSSound.beep()
            return
        }
        guard let url = IcalFiles.pickDestination(named: window.suggestedFilename) else { return }
        guard let text = await ical.exportDocument(window: window) else { return }
        do {
            try IcalFiles.write(text, to: url)
        } catch {
            ical.errorMessage = error.localizedDescription
        }
    }

    /// Whether materialization is running against `bridge` specifically.
    ///
    /// The vault identity is the whole question: a timer running against the
    /// `Core` a switch has already shut down is indistinguishable from no
    /// timer at all, and both are `.running` if you only ask "is it on".
    func routineTimerIsRunning(against bridge: CoreBridge) -> Bool {
        routineTimer == .running(vault: ObjectIdentifier(bridge))
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
        ical = nil
        // The timer lives on the `Core` and goes down with it. Recording that
        // here is what stops ``routineTimerIsRunning(against:)`` from claiming
        // the *next* vault is covered because the previous one was.
        routineTimer = .stopped
        vault = nil
        // Drop it in the same breath. An intent holding a bridge past this
        // point would write into the vault the user just switched away from.
        IntentVault.adopt(nil)
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

/// Whether the core's routine-materialization timer is running, and against
/// which vault.
///
/// The vault is carried in the value rather than kept beside it so the two
/// cannot drift: "running" with no vault attached is exactly the state that let
/// a timer bound to a closed `Core` pass for a working one.
enum RoutineTimerState: Equatable, Sendable {
    case stopped
    case running(vault: ObjectIdentifier)
    /// The seam refused to start it. Held rather than dropped so a screen can
    /// say why recurrence is not advancing.
    case failed(String)

    /// What a settings screen prints.
    var summary: String {
        switch self {
        case .stopped: "Not running"
        case .running: "Running"
        case let .failed(message): "Failed: \(message)"
        }
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
