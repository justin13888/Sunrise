import Foundation

/// The look at the store a reader of the process's one ``AccountModel`` takes
/// before it acts on it.
///
/// Its own file because `AccountModel.swift` sits at SwiftLint's
/// `file_length` warning, which `swiftlint lint --strict` makes an error.
extension AccountModel {
    /// Restore a previous session — unless this model already holds an answer
    /// a second read would overwrite.
    ///
    /// Since #276 one model serves the whole process: every shell window, each
    /// vault a switch opens, and the recovery ceremony. Each of them used to
    /// build a model of its own and call ``restore()`` on it, which is what let
    /// a credential a sign-out could not delete come back as a live bearer:
    /// a fresh read of the store finds the survivor and ``restore()`` publishes
    /// it. On the shared model the same call would do the same thing, so the
    /// readers call this instead, and it reads only in the state a model is
    /// made in — signed out, holding nothing, and with no sign-out on record.
    ///
    /// Everything else is an answer already in hand. A refused sign-out keeps
    /// the user signed out for the rest of the process; a session, a login in
    /// the browser, or a refused read stays as it is. ``restore()`` itself is
    /// unchanged, and the next launch still reads the survivor back — the
    /// process-scoped mitigation #260 chose.
    func restoreIfUnread() {
        guard state == .signedOut, credentials == nil, !signOutRefusedThisSession else { return }
        restore()
    }
}
