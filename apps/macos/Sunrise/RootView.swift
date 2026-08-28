import SwiftUI

/// What the window shows, decided by the session phase.
///
/// The four cases are kept apart on screen because they are apart in reality:
/// a first run has nothing to lose, a locked vault has everything to lose, and
/// a failure is neither.
struct RootView: View {
    @State private var session = SessionModel.standard()

    var body: some View {
        Group {
            switch session.phase {
            case .starting:
                ProgressView("Opening your vault…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .firstRun:
                OnboardingView(create: session.createVault)
            case let .locked(reason):
                LockedView(reason: reason, retry: session.start)
            case .unlocked:
                if let bridge = session.bridge {
                    VaultView(bridge: bridge)
                }
            case let .failed(message):
                ContentUnavailableView(
                    "Sunrise could not start",
                    systemImage: "exclamationmark.triangle",
                    description: Text(message)
                )
            }
        }
        .task { await session.start() }
    }
}

/// The unlocked app. A placeholder until Today and Inbox land.
struct VaultView: View {
    let bridge: CoreBridge

    var body: some View {
        ContentUnavailableView(
            "Vault open",
            systemImage: "checkmark.seal",
            description: Text("Today and Inbox arrive next.")
        )
    }
}
