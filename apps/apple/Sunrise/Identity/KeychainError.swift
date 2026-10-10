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
    /// ``KeychainItem/readAcrossDomains()`` found nothing in the domain it was
    /// addressed to and could not **read** the other one, which refused on its
    /// own terms. Carries that domain's status. Distinct from `unexpected`
    /// because the two name different keychains: `unexpected` out of a read
    /// comes from the store this build resolved to, and this one comes from the
    /// store it fell back to, so a message that did not say which would send a
    /// user to unlock the keychain that is already working.
    ///
    /// It is emphatically **not** "the secret is missing". Answering `nil` here
    /// is what let a temporarily unreadable vault root be reported as a lost
    /// one; this case exists so the distinction survives as far as the screen.
    case otherDomainUnreadable(OSStatus)
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
            L10n.Identity.keychainMalformed
        case let .unexpected(status):
            Self.systemReason(status)
        case let .accessibilityNotRaised(status):
            // "This secret" rather than "the vault key": the vault root was
            // the first caller and is no longer the only one — the OIDC
            // credential raises its class on the same path.
            L10n.Identity.keychainNotRaised(reason: Self.systemReason(status))
        case let .writtenButOtherDomainRefused(status):
            // Leads with what *was* stored: every other message here describes
            // something that did not happen, and this one does not.
            L10n.Identity.keychainWrittenOtherRefused(reason: Self.systemReason(status))
        case let .otherDomainUnreadable(status):
            // Names the *other* keychain, and says the secret may still be
            // there. A message that only relayed the system's sentence would
            // read as a failure of the keychain the app just used
            // successfully, which is the one a user would then go and unlock.
            L10n.Identity.keychainOtherUnreadable(reason: Self.systemReason(status))
        case .migrationUnverified:
            L10n.Identity.keychainMigrationUnverified
        }
    }

    /// The system's own sentence for `status`, or the bare code when it has
    /// none. The code is passed as text so it renders as the digits the
    /// system logs, never regrouped by a locale.
    private static func systemReason(_ status: OSStatus) -> String {
        SecCopyErrorMessageString(status, nil) as String?
            ?? L10n.Identity.keychainError(status: String(status))
    }
}
