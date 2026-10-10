import AppIntents
import Foundation

/// Why an intent could not do what it was asked.
///
/// Every case is a sentence the user reads in Shortcuts, Spotlight or Siri.
/// That is the whole point of the type: an automation that quietly does
/// nothing is worse than one that says it cannot run, and `parity-matrix.md`
/// puts the automation surface on the same footing as the window — so its
/// failures have to be as legible as a screen's.
///
/// `CustomLocalizedStringResourceConvertible` is what the App Intents runtime
/// reads. A plain `Error` surfaces as "The operation couldn't be completed",
/// which names neither the problem nor the fix.
enum IntentError: Swift.Error, CustomLocalizedStringResourceConvertible, Equatable {
    /// The session is open but has no bridge — a state that should not occur.
    case vaultUnavailable
    /// No vault on this Mac yet. Creating one is a decision with consequences
    /// (see `SessionModel.createVault`), so an automation must not make it.
    case noVault
    /// A vault exists and cannot be opened; carries `LockReason.summary`.
    case vaultLocked(String)
    /// Sunrise itself holds the vault lock and has not published its bridge.
    /// See the note on ``IntentVault``.
    case vaultHeldByThisApp
    /// Opening failed; carries the underlying message.
    case vaultFailed(String)
    /// Still opening after the wait. Retrying is the fix.
    case vaultBusy
    /// Nothing but whitespace was handed to capture.
    case nothingToCapture
    /// The parser reduced the line to an empty title.
    case emptyTitle
    /// A task id that no longer resolves — deleted between picking and running.
    case taskNotFound(String)
    /// A focus session is already open on this vault.
    case focusAlreadyRunning(String)
    /// No focus session to end.
    case noFocusRunning
    /// The core refused; carries its own message.
    case core(String)

    /// The sentence, already in the user's language.
    ///
    /// Formatted through `L10n` and handed over as one interpolated argument,
    /// so the resource's own lookup (of the bare `%@` it is keyed by) finds
    /// nothing to translate and passes the sentence through untouched.
    var localizedStringResource: LocalizedStringResource {
        "\(message)"
    }

    private var message: String {
        typealias Failure = L10n.Intents.Failure
        switch self {
        case .vaultUnavailable: return Failure.vaultUnavailable
        case .noVault: return Failure.noVault
        case let .vaultLocked(summary): return Failure.vaultLocked(summary: summary)
        case .vaultHeldByThisApp: return Failure.vaultHeld
        case let .vaultFailed(message): return Failure.vaultFailed(message: message)
        case .vaultBusy: return Failure.vaultBusy
        case .nothingToCapture: return Failure.nothingToCapture
        case .emptyTitle: return Failure.emptyTitle
        case let .taskNotFound(id): return Failure.taskNotFound(id: id)
        case let .focusAlreadyRunning(title): return Failure.focusRunning(title: title)
        case .noFocusRunning: return Failure.noFocus
        case let .core(message): return Failure.core(message: message)
        }
    }
}

extension IntentError {
    /// Wrap anything the seam threw.
    ///
    /// Kept in one place so every intent reports a core failure the same way,
    /// and so an `IntentError` thrown deeper down is not re-wrapped into one
    /// of itself.
    static func wrapping(_ error: any Swift.Error) -> IntentError {
        (error as? IntentError) ?? .core(error.localizedDescription)
    }
}
