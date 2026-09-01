import Foundation
import Security

/// A secret in the macOS Keychain, addressed by service and account.
///
/// This is the reason the Rust side stopped at a mode-0600 file: Swift reaches
/// the Keychain natively, and a Rust binding to it would have been a second,
/// worse implementation of something the platform already does — including
/// the part that matters, which is that the bytes are encrypted at rest under
/// the user's login and are not readable by another user on the machine.
struct KeychainItem: Sendable {
    /// Groups the app's items. Shows as the item's "Where" in Keychain Access.
    let service: String
    /// Distinguishes items within the service.
    let account: String

    /// The stored bytes, or `nil` when there is no such item.
    ///
    /// Throws for every other failure. The distinction is load-bearing: "no
    /// item" means first run, and "the Keychain refused" means a vault whose
    /// key exists and cannot be reached right now. Treating the second as the
    /// first would generate a new root and orphan the existing vault.
    func read() throws -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch status {
        case errSecSuccess:
            guard let data = item as? Data else { throw KeychainError.malformedItem }
            return data
        case errSecItemNotFound:
            return nil
        default:
            throw KeychainError.unexpected(status)
        }
    }

    /// Store `data`, replacing any existing value.
    func write(_ data: Data) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
        let update: [String: Any] = [kSecValueData as String: data]

        let updated = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if updated == errSecSuccess { return }
        guard updated == errSecItemNotFound else { throw KeychainError.unexpected(updated) }

        var insert = query
        insert[kSecValueData as String] = data
        // The vault has to open before the user is asked for anything, which
        // includes before they have unlocked the screen after a reboot on a
        // machine that auto-launches the app.
        insert[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        let added = SecItemAdd(insert as CFDictionary, nil)
        guard added == errSecSuccess else { throw KeychainError.unexpected(added) }
    }

    /// Remove the item. Succeeds when there is nothing to remove.
    func delete() throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.unexpected(status)
        }
    }
}

enum KeychainError: Error, Equatable {
    /// The Keychain returned an item that was not the data we stored.
    case malformedItem
    /// A `Security.framework` status the app has no specific handling for.
    /// The code is kept because it is the only thing that tells a denied
    /// prompt apart from a locked keychain when a user reports the failure.
    case unexpected(OSStatus)
}

extension KeychainError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case .malformedItem:
            "The Keychain item is not in the expected format."
        case let .unexpected(status):
            SecCopyErrorMessageString(status, nil) as String?
                ?? "Keychain error \(status)."
        }
    }
}
