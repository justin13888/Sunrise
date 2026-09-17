import Foundation
import Security
import Synchronization

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
    ///
    /// `targetEnvironment(macCatalyst)` is in the condition because `os(macOS)`
    /// is **false** for Catalyst, which is a process running on macOS where
    /// both keychain implementations exist. `apps/apple/project.yml` declares
    /// no Catalyst target today, so leaving it out would be latent rather than
    /// live — but the claim above is about the operating system, and a Catalyst
    /// build taking the `false` branch would short-circuit to a read of the
    /// wrong store, which is the shape this guard exists to prevent.
    static var domainsAreDistinctStores: Bool {
        #if os(macOS) || targetEnvironment(macCatalyst)
        true
        #else
        false
        #endif
    }

    /// The domain this one is not.
    ///
    /// Only meaningful where ``domainsAreDistinctStores`` is `true`; every
    /// caller here checks that first, because where there is one keychain the
    /// "other" domain is a second name for the same store.
    var other: KeychainDomain {
        switch self {
        case .login: .dataProtection
        case .dataProtection: .login
        }
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

    /// The protection class the probe writes its byte under.
    ///
    /// The **stores'** class, not the platform default, and that is the whole
    /// point of naming it: a probe that writes under `WhenUnlocked` answers
    /// "can this binary reach the data-protection keychain at all", while the
    /// three stores then write under
    /// `…AfterFirstUnlockThisDeviceOnly`. Those are not the same question, and
    /// the one worth asking is the one the stores will actually ask.
    /// `theProbeTestsTheClassTheStoresWriteUnder` pins that this stays equal to
    /// all three stores' `accessibility`.
    static let probeAccessibility = KeychainAccessibility.afterFirstUnlockThisDeviceOnly

    /// How many times ``probe()`` has run in this process.
    ///
    /// Exists so a test can pin that ``current`` is memoised — asserting
    /// `current == probe()` would pass identically with no memoisation at all,
    /// because the probe is deterministic within a process. Counting the calls
    /// is the only thing that tells the two apart.
    static let probeRuns = Atomic<Int>(0)

    /// Ask the platform rather than assume from `#if os(…)`.
    ///
    /// Adds one fixed, **non-secret** byte under ``probeService`` with a random
    /// account, in `.dataProtection` and under ``probeAccessibility``; keeps
    /// the status; deletes whatever it wrote; and answers `.dataProtection`
    /// only on `errSecSuccess`.
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
        probeRuns.add(1, ordering: .relaxed)
        let account = UUID().uuidString
        let status = SecItemAdd(probeInsertQuery(account: account) as CFDictionary, nil)

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

    /// The exact dictionary ``probe()`` hands to `SecItemAdd`.
    ///
    /// Split out so a test can assert what the probe *submits*, rather than what
    /// ``probeAccessibility`` happens to equal.
    /// `theProbeTestsTheClassTheStoresWriteUnder` compares that constant against
    /// the three stores and would pass unchanged if the probe named a different
    /// class here, or dropped the domain key and asked the login keychain —
    /// which would make the answer always `.dataProtection` and send every vault
    /// root to the store this build cannot reach.
    ///
    /// Behaviour cannot stand in for it: the add is refused on the ad-hoc Mac
    /// whatever class it names, succeeds on iOS whatever class it names, and the
    /// probe deletes whatever it wrote either way. ``probe()`` is the one call
    /// site, so what this returns is what the probe does.
    static func probeInsertQuery(account: String) -> [String: Any] {
        var insert: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: probeService,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: probeAccessibility.attribute,
            kSecValueData as String: Data([0])
        ]
        Self.dataProtection.apply(to: &insert)
        return insert
    }
}
