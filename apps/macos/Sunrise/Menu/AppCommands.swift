import SwiftUI

// The menu bar is where `docs/08-features/keyboard.md`'s application-scope
// bindings become visible. Not decoration: the accessibility spec forbids a
// shortcut with no visible affordance, and a menu item is the affordance macOS
// already has — it works with no window open, it is readable by VoiceOver, and
// the system's own Keyboard settings can rebind it.
//
// Every one of these is a `View` rather than a `Button` written inline in the
// scene, so that it has an environment to read `openWindow` from: the menu has
// to work with every window closed, which is exactly when someone reaches for
// ⌘⌥M or ⌘⇧I.

/// What the app menu adds: capture, and the two daily briefs.
///
/// A `View` rather than the buttons written inline, so that it has an
/// environment to read `openWindow` from — the menu has to work with every
/// window closed, which is exactly when someone reaches for ⌘⌥M.
struct AppMenuItems: View {
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
struct CommandMenuItem: View {
    let surfaces: AppSurfaces
    let action: AppAction
    /// Why this item is unavailable, or `nil` when it is available.
    ///
    /// The sentence rather than a `Bool`, because
    /// `docs/10-cross-cutting/accessibility.md` requires every interactive
    /// element to have an accessible name and forbids a state carried by
    /// appearance alone. Dimming is exactly that: a sighted user learns only
    /// "not now" and a VoiceOver user learns nothing at all. The same string
    /// is the tooltip and the accessibility hint, so both audiences get the
    /// reason rather than the symptom.
    let unavailable: String?

    @Environment(\.openWindow) private var openWindow

    init(surfaces: AppSurfaces, action: AppAction, unavailable: String? = nil) {
        self.surfaces = surfaces
        self.action = action
        self.unavailable = unavailable
    }

    var body: some View {
        Button(action.title) {
            openWindow(id: SunriseWindow.main.rawValue)
            surfaces.request(action)
        }
        .keyboardShortcut(for: action)
        .disabled(unavailable != nil)
        .help(unavailable ?? "")
        .accessibilityHint(unavailable ?? "")
    }
}

/// The File menu's top: capture, and a new stream.
struct NewMenuItems: View {
    let surfaces: AppSurfaces

    var body: some View {
        CommandMenuItem(surfaces: surfaces, action: .quickCapture)
        CommandMenuItem(surfaces: surfaces, action: .newStream)
    }
}

/// File → Import Calendar… / Export Calendar ▸ Today | This Week.
///
/// The window is opened before an import runs, because the import's *report*
/// is the point of the feature and the report is a sheet on the window. A
/// notice list nobody can see is the failure mode this menu exists to avoid.
struct IcalMenuItems: View {
    let surfaces: AppSurfaces

    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Import Calendar…") {
            openWindow(id: SunriseWindow.main.rawValue)
            Task { await surfaces.importIcal() }
        }
        .keyboardShortcut("i", modifiers: [.command, .shift])
        .disabled(surfaces.ical == nil)

        Menu("Export Calendar") {
            ForEach(ExportWindow.menuOrder, id: \.self) { window in
                Button(window.menuTitle) {
                    Task { await surfaces.exportIcal(window) }
                }
            }
        }
        .disabled(surfaces.ical == nil)
    }
}

/// File → Print… / Export as PDF….
///
/// Both hand the request to the window, which is the only thing that knows
/// what is on screen to print. `docs/07-clients/parity-matrix.md` marks this a
/// macOS SHOULD.
///
/// Greyed out, with the reason attached, on the screens that have no paper
/// shape — Focus, Routines, the two briefs, and Review's Trends and History.
/// A beep is a poor explanation for a command that can never work here: it
/// says something went wrong without saying what, and it says nothing at all
/// to a screen reader. A disabled item is what macOS itself does with a
/// command that does not apply, and the window keeps
/// ``AppSurfaces/printRefusal`` up to date so it applies to the screen showing
/// now rather than to the app in general.
///
/// A screen that *can* print but happens to be empty stays enabled and beeps.
/// That is a different condition — nothing in the Inbox today, rather than no
/// such thing as printing the Inbox — and a menu item that flickered as tasks
/// came and went would explain less, not more.
struct PrintMenuItems: View {
    let surfaces: AppSurfaces

    var body: some View {
        CommandMenuItem(surfaces: surfaces, action: .printView, unavailable: surfaces.printRefusal)
        CommandMenuItem(surfaces: surfaces, action: .exportPDF, unavailable: surfaces.printRefusal)
    }
}

/// The View menu's additions: the two fixed lists, both searches, and the
/// palette.
struct GoMenuItems: View {
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
