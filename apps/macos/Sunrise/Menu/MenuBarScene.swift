import AppKit
import SwiftUI

/// The menu bar's own label: the app's mark, plus the one number worth a
/// glance.
struct MenuBarLabel: View {
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
struct MenuBarScene: View {
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
