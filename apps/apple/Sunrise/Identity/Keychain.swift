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
    /// Which keychain the item is addressed to. Required for the same reason
    /// `accessibility` is, and with sharper teeth: the two domains are two
    /// different stores on macOS, so a defaulted one is how an item silently
    /// stops being where the last build put it. ``KeychainMigration`` is what
    /// moves an existing item between them.
    let domain: KeychainDomain

    /// The `kSecClass`, service, account and domain every query below starts
    /// from. Collected in one place so the domain cannot be applied to three
    /// of the four operations.
    private var baseQuery: [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
        domain.apply(to: &query)
        return query
    }

    /// The stored bytes, or `nil` when there is no such item.
    ///
    /// Throws for every other failure. The distinction is load-bearing: "no
    /// item" means first run, and "the Keychain refused" means a vault whose
    /// key exists and cannot be reached right now. Treating the second as the
    /// first would generate a new root and orphan the existing vault.
    func read() throws -> Data? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
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

    /// The same secret, addressed to the keychain this one is *not* in.
    ///
    /// Only meaningful where ``KeychainDomain/domainsAreDistinctStores`` is
    /// `true`; both callers check that first.
    private var inOtherDomain: KeychainItem {
        KeychainItem(
            service: service,
            account: account,
            accessibility: accessibility,
            domain: domain.other
        )
    }

    /// ``read()``, and — only when this domain holds nothing — a look in the
    /// other one before answering `nil`.
    ///
    /// This exists because ``KeychainDomain/probe()`` fails open, and failing
    /// open is right *before* a migration and wrong *after* one. Before, a
    /// wrong `.login` answer reads the keychain the item is still in. After,
    /// the item has moved, and a single transient `SecItemAdd` failure at
    /// launch makes the probe answer `.login`, the read find an empty login
    /// keychain, and the app report a missing key — the "looks exactly like a
    /// lost vault" screen, to a user whose vault is intact.
    ///
    /// **The second read can only turn a `nil` into bytes.** It never turns a
    /// success into a failure and never turns a `nil` into a throw: a refusal
    /// from the other domain is swallowed, because it is the domain this build
    /// did **not** resolve to, so a refusal there means there was nothing of
    /// ours to find — and letting it propagate would fail a genuine first run.
    /// The *first* read keeps ``read()``'s contract in full: a refusal there is
    /// still "a key may exist and cannot be reached" and still throws.
    ///
    /// The entitlement is **not** what the `try?` is defending against, and an
    /// earlier revision of this comment said it was. Measured on the ad-hoc Mac
    /// this repository builds, and pinned by
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`: a
    /// `.dataProtection` **query** answers `errSecItemNotFound` (-25300), not
    /// `errSecMissingEntitlement`. The -34018 refusal lands on the *mutating*
    /// calls — `SecItemAdd`, `SecItemUpdate` and `SecItemDelete`. So the swallow
    /// here earns its keep on the entitled Mac, where the other store can be
    /// locked or can refuse a prompt while this one answers, rather than on the
    /// unsigned one, where the second read simply finds nothing.
    func readAcrossDomains() throws -> Data? {
        if let data = try read() { return data }
        guard KeychainDomain.domainsAreDistinctStores else { return nil }
        return try? inOtherDomain.read()
    }

    /// Store `data`, replacing any existing value.
    func write(_ data: Data) throws {
        let query = baseQuery
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

    /// ``write(_:)``, and then the same item in the other domain removed.
    ///
    /// The third side of the symmetry ``readAcrossDomains()`` opened and
    /// ``deleteAcrossDomains()`` closed halfway. Once a `load` can *find* a
    /// secret in the other domain, a `save` that only writes this one leaves
    /// two copies under one `(service, account)` and no rule about which is
    /// current — and the next launch whose ``KeychainDomain/probe()`` answers
    /// correctly walks into ``KeychainMigration``'s one refusal:
    /// ``KeychainError/migrationUnverified``, two different secrets claiming one
    /// name, with no way out that the user can reach.
    ///
    /// The steady state that motivates it needs no user action. An entitled Mac
    /// holds its token in `.dataProtection`; one launch's probe fails open to
    /// `.login` — the premise the whole fallback is built on — the cross-domain
    /// read finds the token and the session restores; then a background renewal
    /// writes the *fresh* token to `.login` while the stale one sits in
    /// `.dataProtection`. Every later launch with a correct probe compares the
    /// two, disagrees, and signs the user out in silence.
    ///
    /// Write first, then delete, for ``KeychainMigration``'s invariant: a
    /// readable copy exists at every instant. The other domain's failure is
    /// swallowed for ``readAcrossDomains()``'s reason — it is the domain this
    /// build did not resolve to, so a refusal there means there was nothing of
    /// ours to remove. This domain's write keeps ``write(_:)``'s contract and
    /// still throws, and nothing is deleted unless it succeeded.
    ///
    /// **Deliberately not folded into ``write(_:)``.**
    /// ``KeychainMigration/migrate(before:)`` writes its destination and only
    /// then deletes its source, and for the credential store the source *is*
    /// this item in the other domain. A `write` that deleted the other domain
    /// would remove the source before the verify step had confirmed the
    /// destination, turning the five-step invariant into the data loss it was
    /// written to prevent. The migration keeps the plain ``write(_:)``; the
    /// stores, whose save is the last word on what the secret is, call this.
    func writeAcrossDomains(_ data: Data) throws {
        try write(data)
        guard KeychainDomain.domainsAreDistinctStores else { return }
        try? inOtherDomain.delete()
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
        let match = baseQuery
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
        let status = SecItemDelete(baseQuery as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.unexpected(status)
        }
    }

    /// ``delete()``, and the same item in the other domain with it.
    ///
    /// The counterpart ``readAcrossDomains()`` requires, and the reason it is
    /// not optional: once a `load` can *find* a secret in the other domain, a
    /// `clear` that only removes this one would leave a signed-out session's
    /// refresh token, or a "forgotten" vault root, readable where the next
    /// launch will look for it. Whatever a read can reach, a clear removes.
    ///
    /// Symmetrically with ``readAcrossDomains()``, the other domain's failure
    /// is swallowed — it is the domain this build did not resolve to, so a
    /// refusal there means there was nothing of ours to remove. This domain's
    /// delete keeps ``delete()``'s contract and still throws.
    ///
    /// **Both are attempted, and only then is this domain's status raised.**
    /// An earlier revision put `try delete()` on its own line ahead of the
    /// cross-domain half, so a refusal here skipped the other domain entirely
    /// and left behind the one copy ``readAcrossDomains()`` can still find —
    /// with `AccountModel.signOut()` swallowing the throw, a user told they
    /// are signed out while a live refresh token waits where the next launch
    /// looks. That is not a hypothetical ordering: on the ad-hoc Mac a
    /// `.dataProtection`-addressed delete answers `errSecMissingEntitlement`
    /// (-34018), so the *whole* cross-domain clear was a no-op for exactly the
    /// item that needed it. This gains no new way to throw: the error raised is
    /// this domain's and only this domain's, which is what ``clear()``'s
    /// callers already handle.
    func deleteAcrossDomains() throws {
        var thisDomainFailure: (any Error)?
        do {
            try delete()
        } catch {
            thisDomainFailure = error
        }
        if KeychainDomain.domainsAreDistinctStores {
            try? inOtherDomain.delete()
        }
        if let thisDomainFailure { throw thisDomainFailure }
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
        case .migrationUnverified:
            "Two different secrets are stored under the same Keychain name, so "
                + "this app cannot tell which one belongs to your vault. "
                + "Nothing has been deleted."
        }
    }
}
