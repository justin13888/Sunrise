import Foundation
import Security
import Testing

@testable import Sunrise

/// The two lines that choose where a secret is stored. Nothing below proves a
/// cross-domain migration works — no build this machine can make reaches the
/// data-protection keychain — but the probe's own answer is checkable, and so
/// is the promise that it leaves nothing behind.
///
/// Serialized because `theProbeDeletesWhateverItWrote` looks at a service the
/// other two cases here write to: two probes in flight at once would let one
/// case see the other's byte in the window between its add and its delete.
///
/// Split out of `KeychainMigrationTests.swift` rather than left as a second
/// struct there: that file sat at exactly the 520-line `file_length` ceiling
/// `swiftlint --strict` enforces, so every case added to it had to be paid for
/// by shortening a comment that recorded an earlier round's correction. The
/// suite is separable on its own terms — it tests where a secret is stored,
/// not how one is moved — so the split is not a concession to the linter. The
/// same precedent is already in the tree at `KeychainError.swift`,
/// `KeychainMigrationVerifyTests.swift` and `KeychainMigrationFallbackTests`'
/// message suite, each citing the same ceiling.
@Suite(.serialized)
struct KeychainDomainTests {
    /// `.login` must add **nothing**, not `kSecUseDataProtectionKeychain:
    /// false`. Writing `false` on iOS is a claim the platform does not honour,
    /// and on macOS it is a second spelling of the default.
    @Test
    func theLoginDomainAddsNothingToAQuery() {
        var query: [String: Any] = [kSecClass as String: kSecClassGenericPassword]
        KeychainDomain.login.apply(to: &query)
        #expect(query.count == 1)
        #expect(!query.keys.contains(kSecUseDataProtectionKeychain as String))
    }

    @Test
    func theDataProtectionDomainAsksForItByName() {
        var query: [String: Any] = [kSecClass as String: kSecClassGenericPassword]
        KeychainDomain.dataProtection.apply(to: &query)
        #expect(query[kSecUseDataProtectionKeychain as String] as? Bool == true)
    }

    /// The probe writes a byte of its own to find out what this binary can reach.
    /// Leaving it behind would put a Sunrise item a user did not ask for in their
    /// Keychain on every cold launch. **Teeth on iOS only**: on macOS the probe's
    /// `SecItemAdd` is refused, so nothing is ever written and dropping `probe()`'s
    /// cleanup loop leaves this green. Declared in `docs/07-clients/desktop.md`; the
    /// assertion is not one of the five an entitlement flips.
    ///
    /// It is also a statement about the **machine** rather than about the probe:
    /// it asks whether anything at all sits under `probeService`, so a Sunrise
    /// app running beside the suite fails it on a tree that is green. It no
    /// longer fails on an orphan left by a killed process — the sweep this same
    /// change made keyed by *service* reclaims exactly that, on the `probe()`
    /// below, before the query runs — and an earlier revision of this paragraph
    /// listed the orphan alongside the racing app after that stopped being true.
    /// `aProbeReclaimsAnOrphanAnEarlierProbeLeftBehind` is the probe-level
    /// assertion, and is what actually pins the sweep.
    @Test
    func theProbeDeletesWhateverItWrote() {
        // Force the memoised probe to have *finished*, so the only one that can
        // be in flight while this looks is the one on the next line. A
        // `static let` blocks every caller until its initialiser returns, which
        // is exactly the barrier needed here.
        _ = KeychainDomain.current
        _ = KeychainDomain.probe()

        for domain in [KeychainDomain.login, .dataProtection] {
            var query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: KeychainDomain.probeService,
                kSecMatchLimit as String: kSecMatchLimitAll
            ]
            domain.apply(to: &query)
            // Deliberately weaker than `== errSecItemNotFound`, which is what both
            // domains answer today: a *query* is answered rather than refused wherever
            // this builds, which `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty` measures.
            #expect(SecItemCopyMatching(query as CFDictionary, nil) != errSecSuccess)
        }
    }

    /// A probe reclaims the residue of probes that never finished.
    ///
    /// This is the assertion `theProbeDeletesWhateverItWrote` cannot make. That
    /// case looks only for the byte the probe it just ran wrote, and on macOS
    /// the probe writes none — so it stays green with the cleanup loop deleted
    /// outright. This one plants the orphan first: an item under `probeService`
    /// with an account no probe will ever mint again, which is exactly what a
    /// process killed between `SecItemAdd` and the cleanup leaves behind. It has
    /// teeth on **both** platforms, because planting into `.login` is something
    /// every build here can do, and it is why the sweep is keyed to the service
    /// rather than to the account the probe just minted — key it back and the
    /// orphan below survives.
    @Test
    func aProbeReclaimsAnOrphanAnEarlierProbeLeftBehind() throws {
        let orphan = KeychainItem(
            service: KeychainDomain.probeService,
            account: "orphan-\(UUID().uuidString)",
            accessibility: KeychainDomain.probeAccessibility,
            domain: .login
        )
        defer { try? orphan.delete() }

        // The same barrier `theProbeDeletesWhateverItWrote` takes, and needed
        // for a sharper reason: `@Suite(.serialized)` orders this suite's cases
        // and nothing else's, so a first touch of `KeychainDomain.current` from
        // another keychain suite can run its memoised probe — and that probe now
        // sweeps the whole service — between the plant below and the assertion
        // that the plant survived. Forcing it to have *finished* here leaves the
        // `probe()` further down as the only one that can be in flight.
        _ = KeychainDomain.current

        try orphan.write(Data([0]))
        #expect(try orphan.read() != nil, "the orphan must exist, or this case proves nothing")

        _ = KeychainDomain.probe()

        #expect(try orphan.read() == nil, "a probe must reclaim its predecessors' residue")
    }

    /// What the probe answers on the configurations this repository builds, and **one
    /// of five** assertions that change the day an entitlement lands — an earlier
    /// revision of this line called it the only one. The other four are in
    /// `KeychainMigrationFallbackTests`: `aDestinationThisBuildCannotReachFallsBackToTheSource`,
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`,
    /// `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther` and
    /// `aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess`. All five are
    /// **rewritten to assert the entitled behaviour**, not deleted and not guarded:
    /// deleting drops the coverage exactly when the path first runs for real, guarding
    /// leaves the entitled build asserting nothing. `docs/07-clients/desktop.md` names
    /// the same five under its Apple-team handoff. iOS has the data-protection keychain
    /// and nothing else; the Mac app is ad-hoc signed with no `keychain-access-groups`
    /// entitlement, so `SecItemAdd` with `kSecUseDataProtectionKeychain` is refused and
    /// the probe falls open to the login keychain — which is why this change is inert.
    ///
    /// The five stay five. `KeychainMigrationFallbackTests`'
    /// `theMigrationsDestinationWriteDoesNotTakeTheSourceWithIt` is a **sixth**
    /// case on the same handoff and deliberately not a sixth *assertion* here:
    /// it is guarded with `.enabled(if:)` rather than asserting today's answer,
    /// because the property it pins — that the migration's destination write
    /// does not delete the source addressed at the other domain — has no true
    /// form on a build that cannot reach `.dataProtection`. These five change
    /// their answer the day a team lands; that one starts running.
    @Test
    func theProbeAnswersWhatThisBuildCanActuallyReach() {
        #if os(iOS)
        #expect(KeychainDomain.probe() == .dataProtection)
        #else
        #expect(KeychainDomain.probe() == .login)
        #endif
    }

    /// Memoised: asked any number of times, the probe runs **once**.
    ///
    /// `current == probe()` was what this case used to say, and it would pass
    /// identically with no memoisation at all — the probe is deterministic
    /// within a process, so equality cannot tell a cached answer from a
    /// recomputed one. Counting the runs can. The equality is kept as the
    /// second assertion, because "memoised on the probe rather than on a
    /// guess" is the other half of the claim.
    @Test
    func theMemoisedDomainRunsTheProbeExactlyOnce() {
        // Force the `static let` to completion first: `swift_once` blocks every
        // caller until the initialiser returns, so after this line no probe of
        // `current`'s can still be in flight and the count is stable.
        let answer = KeychainDomain.current
        let runsBefore = KeychainDomain.probeRuns.load(ordering: .relaxed)
        for _ in 0 ..< 8 { _ = KeychainDomain.current }
        #expect(KeychainDomain.probeRuns.load(ordering: .relaxed) == runsBefore)

        #expect(answer == KeychainDomain.probe())
        #expect(
            KeychainDomain.probeRuns.load(ordering: .relaxed) == runsBefore + 1,
            "the counter must move when the probe really runs, or the assertion above proves nothing"
        )
    }

    /// The `#if` behind `domainsAreDistinctStores`, pinned as a value.
    ///
    /// `KeychainMigration` deletes the source on the strength of this flag, so
    /// a build where it is wrong deletes the only copy of a vault root. The
    /// case is written against the platform directly rather than against the
    /// flag, so flipping the flag's own condition fails here.
    @Test
    func theTwoDomainsAreTwoStoresOnTheMacAndNowhereElse() {
        #if os(macOS) || targetEnvironment(macCatalyst)
        #expect(KeychainDomain.domainsAreDistinctStores)
        #else
        #expect(!KeychainDomain.domainsAreDistinctStores)
        #endif
    }

    /// `other` has to be an involution, because a cross-domain read that
    /// answered with the domain it started in would silently be no fallback.
    @Test
    func theOtherDomainIsTheOneThisIsNot() {
        #expect(KeychainDomain.login.other == .dataProtection)
        #expect(KeychainDomain.dataProtection.other == .login)
    }

    /// The probe must test the reachability that actually matters.
    ///
    /// It writes its byte under the class the three stores write under, not
    /// the platform default: "can this binary reach the data-protection
    /// keychain at all" and "can it store a secret there the way this app
    /// stores secrets" are different questions, and only the second one
    /// decides where a vault root ends up. This pins that the answer stays in
    /// step with all three stores.
    @Test
    func theProbeTestsTheClassTheStoresWriteUnder() {
        #expect(KeychainDomain.probeAccessibility == KeychainVaultRootStore.accessibility)
        #expect(KeychainDomain.probeAccessibility == KeychainCredentialStore.accessibility)
        #expect(KeychainDomain.probeAccessibility == KeychainRelayDeviceIDStore.accessibility)
    }

    /// …and the probe must actually *submit* that class, which the case above
    /// cannot see. It compares a constant against three other constants, so a
    /// `probe()` that named a different class, or that dropped the domain key
    /// and asked the login keychain — making the answer always
    /// `.dataProtection` and sending every vault root to the wrong store —
    /// leaves it green. This reads the dictionary the probe hands `SecItemAdd`.
    @Test
    func theProbeSubmitsTheClassAndTheDomainItClaimsTo() {
        let account = UUID().uuidString
        let insert = KeychainDomain.probeInsertQuery(account: account)

        #expect(insert[kSecAttrAccessible as String] as? String
            == KeychainDomain.probeAccessibility.attribute as String)
        #expect(insert[kSecUseDataProtectionKeychain as String] as? Bool == true)
        #expect(insert[kSecAttrService as String] as? String == KeychainDomain.probeService)
        #expect(insert[kSecAttrAccount as String] as? String == account)
        #expect(insert[kSecValueData as String] as? Data == Data([0]), "one fixed non-secret byte")
    }

    /// `writeAcrossDomains` must never delete the item it has just written.
    ///
    /// The method's second half deletes the *other* domain's copy, and on every
    /// Apple platform but macOS there is no other domain — `.login` and
    /// `.dataProtection` are two names for one store, so an unguarded delete
    /// would remove the secret one line after storing it. That is the same trap
    /// `sourceAndDestinationAreOneItem` exists for, and this is the case that
    /// fails on iOS if the guard is ever dropped.
    ///
    /// On the Mac it pins a second property, and one that only became a
    /// property once the blanket `try?` there was narrowed: the other domain's
    /// refusal is the missing-entitlement status, which
    /// `meansTheOtherStoreWasUnreachable` swallows, so the write still lands.
    /// Drop that status from the predicate and this case fails. What it does
    /// **not** pin is the delete's own effect — `KeychainItem` declares why no
    /// configuration this repository builds can observe that.
    @Test
    func aCrossDomainWriteKeepsWhatItJustWrote() throws {
        let item = KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString).cross-write",
            account: "credentials",
            accessibility: .afterFirstUnlockThisDeviceOnly,
            domain: KeychainDomain.current
        )
        defer { try? item.delete() }

        let secret = Data(repeating: 4, count: 32)
        try item.writeAcrossDomains(secret)
        #expect(try item.read() == secret)

        let renewed = Data(repeating: 5, count: 32)
        try item.writeAcrossDomains(renewed)
        #expect(try item.read() == renewed, "and a rewrite must replace, not remove")
    }
}
