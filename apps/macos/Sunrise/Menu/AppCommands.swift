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
struct PrintMenuItems: View {
    let surfaces: AppSurfaces

    var body: some View {
        CommandMenuItem(surfaces: surfaces, action: .printView)
        CommandMenuItem(surfaces: surfaces, action: .exportPDF)
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
