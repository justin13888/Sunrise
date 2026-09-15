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

    /// Starts the updater, and that is the whole reason this initialiser
    /// exists.
    ///
    /// ``SoftwareUpdate/controller`` is a lazy `static let`, so nothing exists
    /// until something reads it — and the only other reader is a menu item's
    /// action closure, which runs when a user clicks. Left at that,
    /// `SUEnableAutomaticChecks` would be a setting nothing acts on: no
    /// background check is ever scheduled for a user who never opens the menu,
    /// which is most of them. Touching it here starts Sparkle at launch, which
    /// is where Sparkle expects to be started.
    ///
    /// On a build with no `SUPublicEDKey` this is still the right call and
    /// still does nothing: the property evaluates to `nil` and no updater is
    /// created. See ADR-0037.
    init() {
        _ = SoftwareUpdate.controller
    }

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
            // RFC 5545 interchange, in the place macOS puts interchange.
            // `docs/07-clients/parity-matrix.md` marks iCal import/export a
            // macOS MUST, and a seam with no menu item is a feature nobody
            // can reach.
            CommandGroup(replacing: .importExport) {
                IcalMenuItems(surfaces: surfaces)
            }
            CommandGroup(after: .importExport) {
                Divider()
                PrintMenuItems(surfaces: surfaces)
            }
            CommandGroup(after: .appInfo) {
                // Directly under About, which is where every Mac user already
                // looks for it. ADR-0037; on a build with no update-signing
                // public key both items are disabled and say why.
                SoftwareUpdateMenuItems()
                Divider()
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
