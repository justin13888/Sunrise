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

    var localizedStringResource: LocalizedStringResource {
        switch self {
        case .vaultUnavailable:
            "Sunrise reported an open vault with nothing behind it. Reopen Sunrise and try again."
        case .noVault:
            """
            Sunrise has no vault on this Mac yet. Open Sunrise once to create or pair one — \
            an automation will not create it for you.
            """
        case let .vaultLocked(summary):
            "Sunrise could not open its vault. \(summary)"
        case .vaultHeldByThisApp:
            """
            Sunrise already has this vault open and did not offer it to \
            automations. Do this in the Sunrise window, or quit Sunrise and \
            run it again.
            """
        case let .vaultFailed(message):
            "Sunrise could not open its vault: \(message)"
        case .vaultBusy:
            "Sunrise is still opening its vault. Try again in a moment."
        case .nothingToCapture:
            "There was nothing to capture."
        case .emptyTitle:
            "That capture line has no title left once its tags are read."
        case let .taskNotFound(id):
            "That task is no longer in Sunrise (\(id))."
        case let .focusAlreadyRunning(title):
            "A focus session is already running on “\(title)”. End it before starting another."
        case .noFocusRunning:
            "No focus session is running."
        case let .core(message):
            "Sunrise could not complete that: \(message)"
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
