import SwiftUI

/// A vault that exists and cannot be opened.
///
/// Deliberately offers no way to create a new one. The obvious repair is the
/// destructive one, and the screen a user reaches in a panic is the worst
/// place to put it.
struct LockedView: View {
    let reason: SessionModel.LockReason
    let retry: () async -> Void

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "lock.trianglebadge.exclamationmark")
                .font(.system(size: 44))
                .foregroundStyle(.orange)
            Text("Sunrise is locked")
                .font(.title.weight(.semibold))
            Text(reason.summary)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 460)

            HStack(spacing: 12) {
                Button("Try again") { Task { await retry() } }
                    .buttonStyle(.borderedProminent)
                if case .keychainUnavailable = reason {
                    Button("Open Keychain Access") {
                        NSWorkspace.shared.open(
                            URL(filePath: "/System/Applications/Utilities/Keychain Access.app")
                        )
                    }
                }
            }
            .controlSize(.large)
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

#Preview {
    LockedView(reason: .keyMissingForExistingVault, retry: {})
}
