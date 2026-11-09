import Foundation

/// Whether the core's routine-materialization timer is running, and against
/// which vault.
///
/// The vault is carried in the value rather than kept beside it so the two
/// cannot drift: "running" with no vault attached is exactly the state that let
/// a timer bound to a closed `Core` pass for a working one.
enum RoutineTimerState: Equatable, Sendable {
    case stopped
    case running(vault: ObjectIdentifier)
    /// The seam refused to start it. Held rather than dropped so a screen can
    /// say why recurrence is not advancing.
    case failed(String)

    /// What a settings screen prints.
    var summary: String {
        switch self {
        case .stopped: L10n.App.routineTimerStopped
        case .running: L10n.App.routineTimerRunning
        case let .failed(message): L10n.App.routineTimerFailed(message: message)
        }
    }
}

/// Why a capture could not be written.
enum CaptureError: Error, Equatable, LocalizedError {
    /// No vault is open — the window between one being closed and the next
    /// being opened, which multi-account switching made reachable.
    case noOpenVault

    var errorDescription: String? {
        switch self {
        case .noOpenVault:
            L10n.App.noOpenVault
        }
    }
}
