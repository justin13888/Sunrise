import SwiftUI

/// The unlocked app on iOS.
///
/// **A placeholder, and deliberately an obvious one.** This commit exists to
/// prove two things and no more: that the shared sources compile for iOS, and
/// that the whole `SunriseTests` bundle passes on the simulator. Standing up a
/// real tab shell in the same commit would have meant a green test run whose
/// green came from somewhere nobody could point at.
///
/// The real shell — `TabView(.sidebarAdaptable)`, a `NavigationStack` per tab,
/// the capture sheet and the `Destination` routing — lands next.
struct VaultTabs: View {
    let bridge: CoreBridge
    let session: SessionModel
    let surfaces: AppSurfaces

    var body: some View {
        ContentUnavailableView(
            "Vault open",
            systemImage: "sun.max",
            description: Text("The iOS shell lands in the next commit.")
        )
    }
}

/// What ``RootView`` builds once the vault is open — see the macOS twin in
/// `macOS/VaultWindow.swift` for what this name is for.
typealias VaultShell = VaultTabs
