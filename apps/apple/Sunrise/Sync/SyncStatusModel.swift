import Foundation

/// Polls the core for sync health.
///
/// A poll rather than a subscription because sync state changes without any
/// domain change: a dropped socket produces no `ChangeEvent`, so a badge
/// driven only by the change stream would sit on "Synced" through an outage.
@MainActor
@Observable
final class SyncStatusModel {
    private(set) var snapshot: SyncSnapshot?

    /// What to draw. Never `nil`: before the first answer it reads "Checking…"
    /// rather than an empty space that looks like "fine".
    var presentation: SyncPresentation {
        snapshot.map(SyncPresentation.init) ?? .unknown
    }

    func refresh(from bridge: CoreBridge) async {
        guard case let .syncStatus(status)? = try? await bridge.query(.syncStatus) else { return }
        snapshot = status
    }

    /// Poll until cancelled.
    func poll(from bridge: CoreBridge, every interval: Duration = .seconds(2)) async {
        while !Task.isCancelled {
            await refresh(from: bridge)
            try? await Task.sleep(for: interval)
        }
    }
}
