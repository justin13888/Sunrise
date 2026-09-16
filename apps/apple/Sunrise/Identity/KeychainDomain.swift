import Foundation
import Security

/// Which of Apple's two keychain implementations an item is addressed to.
///
/// Spelled as an enum beside ``KeychainAccessibility`` and for the same reason:
/// the difference between the two is one a reviewer should not have to recall,
/// and `kSecUseDataProtectionKeychain` is a `CFString` that cannot be stored in
/// a `Sendable` struct.
///
/// The distinction only has teeth on macOS. `docs/07-clients/desktop.md`
/// §The data-protection keychain is not a one-line entitlement records what was
/// measured: without the `keychain-access-groups` entitlement the Mac app uses
/// the file-based login keychain, which accepts `kSecAttrAccessible` on
/// `SecItemAdd`, stores nothing, and returns no such attribute on read — so the
/// `ThisDeviceOnly` class every Sunrise Keychain item declares is inert there,
/// and a Mac moved by Migration Assistant carries the vault root with it.
enum KeychainDomain: Sendable, Equatable {
    /// The macOS file-based login keychain. Implements no protection classes.
    case login
    /// The data-protection keychain — the only keychain iOS has, and the one
    /// macOS reaches only with the entitlement this repository cannot sign for.
    case dataProtection

    /// Add whatever this domain needs to a `Security.framework` query.
    ///
    /// `.dataProtection` sets `kSecUseDataProtectionKeychain` to `true`.
    /// `.login` sets **nothing at all**, rather than setting the same key to
    /// `false`, and the asymmetry is deliberate. Absence is already the
    /// platform default on macOS, so `false` would only add a second spelling
    /// of the status quo; and on iOS, where the data-protection keychain is the
    /// only keychain there is, `false` is a claim the platform does not honour.
    /// A key that is never written cannot be misread.
    func apply(to query: inout [String: Any]) {
        switch self {
        case .login:
            break
        case .dataProtection:
            query[kSecUseDataProtectionKeychain as String] = true
        }
    }

    /// Whether ``login`` and ``dataProtection`` name two *different* stores.
    ///
    /// This is the one fact here that genuinely belongs to the operating system
    /// rather than to the app's signature, which is why it is the one thing
    /// spelled with a `#if`. macOS has two keychain implementations, and a
    /// query reaches the legacy file-based one unless
    /// `kSecUseDataProtectionKeychain` says otherwise. Every other Apple
    /// platform has exactly one keychain and it *is* the data-protection
    /// keychain: there is no login keychain there to move an item out of.
    ///
    /// ``KeychainMigration`` reads this before it deletes anything. Without it
    /// an iOS launch would treat the source and the destination as two names
    /// for one item, find them equal, verify them against each other and then
    /// delete the only copy — which is precisely the "looks exactly like a lost
    /// vault" failure the migration exists to prevent, caused by the migration.
    static var domainsAreDistinctStores: Bool {
        #if os(macOS)
        true
        #else
        false
        #endif
    }

    /// The domain this process stores its secrets in, asked once.
    ///
    /// Memoised because the answer cannot change inside a process — it is
    /// decided by the binary's entitlements — and because three stores ask it
    /// from their initialisers. A `static let` is initialised lazily and
    /// exactly once, which is the whole requirement.
    static let current: KeychainDomain = probe()

    /// The service the probe writes under. It never holds anything of the
    /// user's, which is what makes writing it on every cold launch acceptable.
    static let probeService = "dev.sunrise.Sunrise.keychain-domain-probe"

    /// Ask the platform rather than assume from `#if os(…)`.
    ///
    /// Adds one fixed, **non-secret** byte under ``probeService`` with a random
    /// account, in `.dataProtection`; keeps the status; deletes whatever it
    /// wrote; and answers `.dataProtection` only on `errSecSuccess`.
    ///
    /// A compile-time constant was the obvious alternative and is wrong:
    /// "macOS cannot reach the data-protection keychain" is a fact about the
    /// *signature*, not about the operating system — `apps/apple/project.yml`
    /// sets `CODE_SIGN_IDENTITY: "-"` and an empty `DEVELOPMENT_TEAM`, and on
    /// the day a real team lands the same source has to start answering
    /// differently. An `#if` would be a second site that must change and the
    /// first that would be forgotten.
    ///
    /// **It never throws.** It fails open, to the weaker-but-reachable
    /// keychain, which is deliberately the opposite direction from
    /// ``KeychainItem/upgradeAccessibilityIfNeeded()``: a refused *raise*
    /// leaves a readable secret whose guarantee is wrong and is worth failing
    /// the load over, while a refused *domain* would leave a secret the app
    /// cannot see at all.
    static func probe() -> KeychainDomain {
        let account = UUID().uuidString
        var insert: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: probeService,
            kSecAttrAccount as String: account,
            kSecValueData as String: Data([0])
        ]
        Self.dataProtection.apply(to: &insert)
        let status = SecItemAdd(insert as CFDictionary, nil)

        // Deleted under both domains rather than only the one it was offered
        // to. On a platform where the two are one store the add landed under
        // the other name as well, and a probe that leaves residue in a
        // developer's login keychain on every launch is not one worth having.
        for domain in [Self.dataProtection, .login] {
            var delete: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: probeService,
                kSecAttrAccount as String: account
            ]
            domain.apply(to: &delete)
            _ = SecItemDelete(delete as CFDictionary)
        }

        return status == errSecSuccess ? .dataProtection : .login
    }
}
