import SwiftUI

/// The iOS and iPadOS client.
///
/// One scene, because a phone has one. The Mac's other two surfaces — the
/// borderless capture panel on a global hotkey, and the menu bar item — have
/// no counterpart here and are not emulated: iOS's equivalents are the capture
/// sheet, the Control Center control, the widget and the App Shortcut, all of
/// which reach the same `sunrise://capture` route.
///
/// `RootView` below is the *same* file the Mac uses. The session, the phases
/// it moves through, deep-link delivery and the vault it opens are shared; the
/// only thing this scene chooses is what `VaultShell` resolves to.
@main
struct SunriseiOSApp: App {
    /// Owned here for the reason the Mac owns it at app scope: the core holds
    /// the vault lock for as long as it runs and a second `Core::open` on the
    /// same directory is refused, so every surface in the process has to share
    /// one open vault.
    @State private var session: SessionModel
    /// Publishing into the App Group container the Home and Lock Screen
    /// widgets read. `project.yml` names the group.
    @State private var surfaces: AppSurfaces
    @Environment(\.scenePhase) private var scenePhase
    /// Background task registration, the APNs token and the silent push —
    /// the OS entry points a scene has no modifier for.
    @UIApplicationDelegateAdaptor(SunriseAppDelegate.self) private var appDelegate

    /// Builds the session here rather than in the property declarations so
    /// the background host is bound to the same instance before launch
    /// finishes: a refresh task or a silent push can launch the app with no
    /// window at all, and it has to sync *this* session's vault.
    init() {
        let session = SessionModel.standard()
        let surfaces = AppSurfaces(widgets: .appGroup())
        _session = State(initialValue: session)
        _surfaces = State(initialValue: surfaces)
        BackgroundHost.shared.attach(session: session, surfaces: surfaces)
    }

    var body: some Scene {
        WindowGroup {
            RootView(session: session, surfaces: surfaces)
        }
        // An iPad with a hardware keyboard gets the Mac's ⌘ bindings, the
        // palette and the cheat sheet (`docs/08-features/keyboard.md` Rule 3).
        .commands { KeyCommandMenus(surfaces: surfaces) }
        // An iOS app is suspended in the background, so its change feed and
        // its timer stop with it. Coming back is the moment the snapshot is
        // most likely to be stale — a day that rolled over, a sync that
        // landed while it slept — and a re-read that finds nothing new
        // redraws nothing.
        //
        // Going the other way is the moment to ask the OS for the next
        // background refresh and maintenance window, so a suspended app is
        // still woken to sync (`docs/07-clients/mobile-ios.md` §Background
        // sync).
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .active:
                surfaces.widgets?.refresh()
                // Shared from another app while Sunrise was in the background;
                // the vault's opening files it too, for a cold launch.
                if let vault = surfaces.vault {
                    Task { await ShareInbox.shared.file(into: vault) }
                }
            case .background:
                BackgroundHost.shared.sync?.scheduleRefresh()
                BackgroundHost.shared.sync?.scheduleMaintenance()
            default: break
            }
        }
    }
}
