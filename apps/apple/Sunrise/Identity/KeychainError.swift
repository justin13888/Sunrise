import Foundation
import Security

/// Every way a ``KeychainItem`` or a ``KeychainMigration`` can refuse, and the
/// sentence each one shows a user.
///
/// Split out of `Keychain.swift` rather than left beside `KeychainItem`: that
/// file sits on the 520-line `file_length` ceiling `swiftlint --strict`
/// enforces, and five rounds of review have now had to pay for a corrected
/// sentence there by shortening an unrelated one. The type is separable on its
/// own terms — it names failures, it performs none — so the split is not a
/// concession to the linter.
enum KeychainError: Error, Equatable {
    /// The Keychain returned an item that was not the data we stored.
    case malformedItem
    /// A `Security.framework` status the app has no specific handling for.
    /// The code is kept because it is the only thing that tells a denied
    /// prompt apart from a locked keychain when a user reports the failure.
    case unexpected(OSStatus)
    /// The item exists under a weaker protection class than this build
    /// requires and the Keychain refused to change it. Distinct from
    /// `unexpected` because the secret is readable and the *guarantee* is
    /// what failed, which is what a support reader needs to be told.
    case accessibilityNotRaised(OSStatus)
    /// ``KeychainItem/writeAcrossDomains(_:)`` **stored the bytes** and then
    /// could not take away the copy in the other keychain, which refused on its
    /// own terms. Carries that domain's status. Distinct from `unexpected`
    /// because the two say opposite things about the one question a `save`'s
    /// caller asks — `unexpected` out of a write means nothing was stored, this
    /// one means the write *succeeded* and the cleanup did not. See
    /// ``KeychainItem/writeAcrossDomains(_:)`` for the two sessions the missing
    /// distinction cost.
    case writtenButOtherDomainRefused(OSStatus)
    /// A ``KeychainMigration`` found a secret already sitting at its
    /// destination whose bytes are **not** the source's, so two different
    /// secrets claim one `(service, account)` and nothing here can tell which
    /// one the vault was sealed with. Carries no status code because no
    /// `Security.framework` call failed: both reads succeeded and disagreed.
    ///
    /// This is the one migration failure that refuses the load. Every other
    /// one falls back to the source item, which is still readable and still
    /// correct — locking a user out over a destination problem would be a
    /// strictly worse trade than the one ``accessibilityNotRaised(_:)`` takes.
    case migrationUnverified
}

extension KeychainError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case .malformedItem:
            "The Keychain item is not in the expected format."
        case let .unexpected(status):
            SecCopyErrorMessageString(status, nil) as String?
                ?? "Keychain error \(status)."
        case let .accessibilityNotRaised(status):
            // "This secret" rather than "the vault key": the vault root was
            // the first caller and is no longer the only one — the OIDC
            // credential raises its class on the same path.
            "This secret is stored under an older, weaker Keychain protection "
                + "class and the Keychain would not change it: "
                + (SecCopyErrorMessageString(status, nil) as String?
                    ?? "Keychain error \(status).")
        case let .writtenButOtherDomainRefused(status):
            // Leads with what *was* stored: every other message here describes
            // something that did not happen, and this one does not.
            "This secret was saved, but an older copy of it in your other "
                + "keychain could not be removed: "
                + (SecCopyErrorMessageString(status, nil) as String?
                    ?? "Keychain error \(status).")
        case .migrationUnverified:
            "Two different secrets are stored under the same Keychain name, so "
                + "this app cannot tell which one belongs to your vault. "
                + "Nothing has been deleted."
        }
    }
}
