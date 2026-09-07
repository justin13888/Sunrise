import Foundation
import Security

/// When the Keychain will release an item, and — the half this app is
/// deciding — whether the item may leave the device inside a backup.
///
/// Spelled as an enum rather than passing `kSecAttrAccessible` values around
/// because the two constants below differ in one respect that no reviewer
/// should have to recall from memory, and because the `CFString` constants
/// cannot be stored in a `Sendable` struct.
enum KeychainAccessibility: Sendable, Equatable {
    /// Readable once the device has been unlocked at least once since boot,
    /// and **carried in an encrypted device backup**: an item in this class
    /// restores onto different hardware.
    case afterFirstUnlock
    /// The same availability, minus the backup. iOS keeps the item wrapped
    /// under the device's UID key, so an encrypted backup cannot re-key it for
    /// another device: it survives a restore onto the *same* device and never
    /// appears on a different one.
    case afterFirstUnlockThisDeviceOnly

    /// The `kSecAttrAccessible` value.
    var attribute: CFString {
        switch self {
        case .afterFirstUnlock: kSecAttrAccessibleAfterFirstUnlock
        case .afterFirstUnlockThisDeviceOnly: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        }
    }
}

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
    /// The protection class the bytes are stored under. Required rather than
    /// defaulted: whether a secret may travel in a backup is a decision each
    /// caller has to take, and a default is how one gets taken by accident.
    let accessibility: KeychainAccessibility

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
        // The protection class rides along on the update, not only on the
        // insert: `SecItemUpdate` changes exactly the attributes it is handed,
        // so an item added by an older build would otherwise keep that build's
        // class for the life of the installation.
        let update: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: accessibility.attribute
        ]

        let updated = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if updated == errSecSuccess { return }
        guard updated == errSecItemNotFound else { throw KeychainError.unexpected(updated) }

        var insert = query
        insert[kSecValueData as String] = data
        insert[kSecAttrAccessible as String] = accessibility.attribute
        let added = SecItemAdd(insert as CFDictionary, nil)
        guard added == errSecSuccess else { throw KeychainError.unexpected(added) }
    }

    /// Raise an existing item to `accessibility`, if it is not there already.
    ///
    /// The vault root is written once and read on every launch afterwards, so
    /// nothing on the ordinary path ever rewrites it: without this, an item
    /// added under a weaker class by an earlier build would keep that class
    /// forever, and the upgrade that tightened the class would be a no-op for
    /// exactly the installations that already exist. Call it before the read.
    ///
    /// A keychain that does not implement protection classes returns no
    /// `kSecAttrAccessible` and is left alone. That is the macOS file-based
    /// login keychain, which an app without the data-protection entitlement
    /// still uses: it accepts the attribute on add, stores nothing, and
    /// reports nothing back. There is no class there to raise or lower.
    func upgradeAccessibilityIfNeeded() throws {
        let match: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
        var query = match
        query[kSecReturnAttributes as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var found: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &found)
        switch status {
        case errSecSuccess: break
        case errSecItemNotFound: return
        default: throw KeychainError.unexpected(status)
        }

        guard let attributes = found as? [String: Any],
              let current = attributes[kSecAttrAccessible as String] as? String,
              current != accessibility.attribute as String
        else { return }

        // Failing the load is the deliberate answer to a refusal here. The
        // alternative is to hand back a key that is still in the backup-bearing
        // class while telling the user nothing, and a security downgrade that
        // survives its own fix is the thing this method exists to prevent. No
        // data is at risk: the item is untouched, nothing generates a new root
        // on a throw, and the next launch tries again.
        let updated = SecItemUpdate(
            match as CFDictionary,
            [kSecAttrAccessible as String: accessibility.attribute] as CFDictionary
        )
        guard updated == errSecSuccess else {
            throw KeychainError.accessibilityNotRaised(updated)
        }
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
    /// The item exists under a weaker protection class than this build
    /// requires and the Keychain refused to change it. Distinct from
    /// `unexpected` because the secret is readable and the *guarantee* is
    /// what failed, which is what a support reader needs to be told.
    case accessibilityNotRaised(OSStatus)
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
            "The vault key is stored under an older, weaker Keychain protection "
                + "class and the Keychain would not change it: "
                + (SecCopyErrorMessageString(status, nil) as String?
                    ?? "Keychain error \(status).")
        }
    }
}
