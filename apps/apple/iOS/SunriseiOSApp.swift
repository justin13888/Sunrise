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
    @State private var session = SessionModel.standard()
    @State private var surfaces = AppSurfaces()

    var body: some Scene {
        WindowGroup {
            RootView(session: session, surfaces: surfaces)
        }
    }
}
