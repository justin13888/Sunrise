import SwiftUI

/// The macOS client.
///
/// One window, no document model: the vault is the document and there is
/// exactly one of it per machine. ADR-0019 records why this is a native
/// SwiftUI app rather than the Tauri desktop shell the docs used to specify.
@main
struct SunriseApp: App {
    var body: some Scene {
        WindowGroup("Sunrise") {
            RootView()
                .frame(minWidth: 720, minHeight: 480)
        }
        .defaultSize(width: 1000, height: 700)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }
    }
}
