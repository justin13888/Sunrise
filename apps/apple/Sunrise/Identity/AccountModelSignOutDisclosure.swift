import Foundation

/// What the Account screen renders about the last sign-out — the vocabulary
/// and the rules, with no stored state of its own.
///
/// Split out of `AccountModel.swift` rather than written here, and the reason
/// is a gate. Two changes landed on that file for unrelated reasons: the
/// refused-sign-out disclosure this extension holds, and the shape-of-refusal
/// test that decides whether
/// ``AccountModel/signIn(issuer:clientID:deviceID:nowMs:)`` takes a second
/// look at the store before it opens a browser. Together they carried the file
/// to 584 lines against SwiftLint's `file_length` warning of 520, which
/// `swiftlint lint --strict` makes an error. Neither change is the one that
/// should shrink; the file was holding two subjects.
///
/// This is the half that can move, because none of it writes. Every member
/// here derives from ``AccountModel/signOutResidue`` and
/// ``AccountModel/state``, whose getters are internal. The mutators stay with
/// the model: ``AccountModel/signOut()``,
/// ``AccountModel/dismissSignOutIncomplete()`` and
/// ``AccountModel/dismissSignOutRetry()`` assign `signOutResidue`, whose
/// setter is `private` and so reachable only from the file that declares it.
///
/// `project.yml` globs `Sunrise/`, so a file here joins both the macOS and the
/// iOS target with no project edit.
extension AccountModel {
    /// What a refused sign-out has left behind, and how far the user has got
    /// with it.
    ///
    /// One value rather than a flag per row. As two independent booleans the
    /// message and the retry were simultaneously true in the ordinary refused
    /// state, so nothing here said which of the two rows the screen should
    /// carry and the view had to invent the rule — off the model, where no
    /// test reaches it. Every combination that can exist is a case here, and
    /// no two of them are true at once.
    enum SignOutResidue: Equatable {
        /// The Keychain let go of the credential, or was never asked.
        case none
        /// It refused, and the message has not been acknowledged yet. The
        /// payload is the Keychain's own localized prose.
        case unread(String)
        /// The message was acknowledged. The credential it named is still
        /// stored, so the control that re-runs the removal stays.
        case acknowledged
        /// The retry was dismissed too, so the screen says nothing more about
        /// this refusal. A sign-out refused again starts the sequence over.
        case retired
    }

    /// What the Account screen renders about the last sign-out — the whole of
    /// it, as one value.
    ///
    /// The view switches over this and keeps no rule of its own, so it cannot
    /// render both rows, nor neither, nor the wrong one of the two when the
    /// conditions behind them overlap.
    enum SignOutDisclosure: Equatable {
        /// The screen says nothing about the last sign-out.
        case none
        /// The full disclosure, carrying the Keychain's own message.
        case incomplete(String)
        /// The message has been read; the credential it named is still
        /// stored, so the way to act on it is still offered.
        case retry
    }
    /// The message a refused sign-out left unread, or `nil` once it has been
    /// acknowledged, retired, or made untrue by a replaced credential.
    var signOutIncomplete: String? {
        if case let .unread(message) = signOutResidue { message } else { nil }
    }

    /// Whether the Keychain has refused a sign-out whose credential is still
    /// the stored one. Unlike ``signOutIncomplete`` this outlives
    /// ``dismissSignOutIncomplete()`` and ``dismissSignOutRetry()``:
    /// acknowledging a message does not remove the credential it is about.
    var signOutRefusedThisSession: Bool { signOutResidue != .none }

    /// What the Account screen renders about the last sign-out.
    ///
    /// Which of the cases wins is decided here rather than at the render site,
    /// so a change to it fails a test instead of only a screenshot. The
    /// message is suppressed under `.signedIn`, where its text would predict a
    /// re-admission that has already happened, and stands aside under
    /// `.awaitingBrowser` for the browser in front of it. The retry is
    /// suppressed under `.signedIn` alone, whose own arm carries a **Sign
    /// out**: an abandoned login parks `.awaitingBrowser` for
    /// ``redirectTimeoutMs`` with no cancel and no other control that reaches
    /// `store.clear()`.
    var signOutDisclosure: SignOutDisclosure {
        switch signOutResidue {
        case .none, .retired:
            return .none
        case let .unread(message) where stateTheMessageIsTrueIn:
            return .incomplete(message)
        case .unread, .acknowledged:
            return stateTheRetryIsUsefulIn ? .retry : .none
        }
    }

    /// Whether the screen should carry a plain **Sign out** of its own,
    /// alongside whatever ``signOutDisclosure`` says — which, where this is
    /// `true`, is nothing.
    ///
    /// ``dismissSignOutRetry()`` retires the disclosure, not the credential.
    /// Without this the retired state is the end state this whole change
    /// exists to remove: the token is still in the Keychain, the next launch
    /// reads it back, and no control on the Account screen reaches
    /// `store.clear()` — **Sign in…** needs a `save()` the same lock refuses,
    /// and the `.signedIn` arm's own **Sign out** is a state away. It is
    /// reached by consent here rather than by a dismissal that destroyed the
    /// only retry, which is a real difference and not a difference in end
    /// state. This is the control that keeps it from being a trap.
    ///
    /// It carries no warning text, because the user has said twice that they
    /// do not want to be told again; and unlike the row it replaces it does
    /// clear, because a sign-out that succeeds sets the residue to
    /// ``SignOutResidue/none`` and this with it.
    ///
    /// It also stands under a `.failed` that still holds a credential: an
    /// expired session whose renewal failed keeps its refresh token so the
    /// next tick can try again (see
    /// ``refreshIfNeeded(issuer:clientID:nowMs:)``), and that row's only
    /// other control is **Try again**, which opens the browser.
    var offersBareSignOut: Bool {
        guard signOutDisclosure == .none else { return false }
        if case .failed = state, credentials != nil { return true }
        guard signOutRefusedThisSession else { return false }
        switch state {
        // The two settled states. Not `.signedIn`, whose arm has a **Sign
        // out** already, and not `.awaitingBrowser`, where a login is in front
        // of the user and a retired residue is the one thing they asked to
        // stop hearing about.
        case .signedOut, .failed: return true
        case .signedIn, .awaitingBrowser: return false
        }
    }

    /// The states the message's own text is true and wanted in.
    private var stateTheMessageIsTrueIn: Bool {
        switch state {
        case .signedOut, .failed: true
        case .signedIn, .awaitingBrowser: false
        }
    }

    /// The states a retry is worth offering in: every one where the Account
    /// screen has no other control that reaches ``signOut()``.
    private var stateTheRetryIsUsefulIn: Bool {
        switch state {
        case .signedOut, .failed, .awaitingBrowser: true
        case .signedIn: false
        }
    }
}
