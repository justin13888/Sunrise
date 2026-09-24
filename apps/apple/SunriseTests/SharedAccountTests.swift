import Foundation
import Security
import Testing

@testable import Sunrise

/// One signed-in account per process, and what that buys (#276).
///
/// The recovery ceremony used to build an ``AccountModel`` of its own and
/// `restore()` it. A sign-out the Keychain refused to complete leaves the
/// credential stored, so that fresh model read the survivor back and handed it
/// to `bootstrapAccount` as a live bearer — while the Account screen, holding a
/// different model, said the user was signed out.
///
/// The helpers come from ``AccountModelTests``' file — `StubCredentialStore`
/// and `credentials(accessToken:)` — and `StubRootStore` from
/// ``SessionModelTests``' file.
@MainActor
struct SharedAccountTests {
    private static let refusal = KeychainError.unexpected(errSecInteractionNotAllowed)

    /// The Keychain's state after a refused sign-out: the credential is still
    /// there, and every `clear()` is refused.
    private func lockedStore() -> StubCredentialStore {
        StubCredentialStore(value: credentials(accessToken: "access-old"), clearFailure: Self.refusal)
    }

    private func model(store: StubCredentialStore) -> AccountModel {
        AccountModel(store: store, makeDriver: { _, _ in StubLoginDriver() }, openURL: { _ in })
    }

    // MARK: - The first look

    @Test
    func aFreshModelTakesItsFirstLook() {
        let account = model(store: lockedStore())

        account.restoreIfUnread()

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.accessToken == "access-old")
    }

    /// **The defect.** `restore()` re-admits the survivor — that is the
    /// cross-launch behaviour #260 accepted, and `AccountModelTests` pins it.
    /// A second reader in the same process must not.
    @Test
    func aRefusedSignOutIsNotUndoneByAnotherReaderInTheSameProcess() {
        let store = lockedStore()
        let account = model(store: store)
        account.restoreIfUnread()
        account.signOut()
        #expect(store.stored != nil, "the Keychain kept the credential")

        account.restoreIfUnread()

        #expect(account.state == .signedOut)
        #expect(account.accessToken == nil, "no bearer for anything that reads this model")
        #expect(account.signOutIncomplete != nil, "and the disclosure is still there to render")
    }

    /// Still true once the user has dismissed both rows: the credential they
    /// were told about is still stored.
    @Test
    func aRetiredRefusalStillKeepsTheSurvivorOut() {
        let account = model(store: lockedStore())
        account.restoreIfUnread()
        account.signOut()
        account.dismissSignOutIncomplete()
        account.dismissSignOutRetry()

        account.restoreIfUnread()

        #expect(account.accessToken == nil)
    }

    /// A refused *read* is an answer too: its remedy is the Account screen's
    /// Try again, which `signIn` turns into a second look — not a re-read by
    /// whichever window opened next.
    @Test
    func aRefusedReadIsLeftForTryAgain() {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            loadError: KeychainError.otherDomainUnreadable(errSecInteractionNotAllowed)
        )
        let account = model(store: store)
        account.restoreIfUnread()
        guard case .failed = account.state else {
            Issue.record("a refused read reports, got \(account.state)")
            return
        }
        store.loadError = nil

        account.restoreIfUnread()

        guard case .failed = account.state else {
            Issue.record("the second reader must not replace the report, got \(account.state)")
            return
        }
    }

    /// A signed-in model keeps its session rather than re-reading over it.
    @Test
    func aSessionInHandIsNotReread() {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store)
        account.restoreIfUnread()
        try? store.save(credentials(accessToken: "access-other", expiresAtMs: 9_000))

        account.restoreIfUnread()

        #expect(account.accessToken == "access-old")
    }

    // MARK: - The ceremony

    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory.appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    /// A private defaults suite, so no case writes the developer's own settings.
    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// Settings under which the ceremony gets as far as asking for a bearer.
    private func configure(_ defaults: UserDefaults) {
        let settings = AppSettings(defaults: defaults)
        settings.relayURL = "wss://relay.invalid/sync"
        settings.accountEmail = "user@example.com"
    }

    private func configuredDefaults() throws -> UserDefaults {
        let defaults = try scratchDefaults()
        configure(defaults)
        return defaults
    }

    /// The issue's sequence end to end: signed in, a sign-out the Keychain
    /// refused, then a vault created — and the ceremony it presents asks for a
    /// sign-in rather than uploading under the credential that survived.
    @Test
    func theCeremonyUsesTheSessionsAccountAndSoHonoursARefusedSignOut() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = lockedStore()
        let account = model(store: store)
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(),
            appVersion: "test",
            settingsDefaults: try configuredDefaults(),
            account: account
        )
        #expect(session.account === account)
        account.restoreIfUnread()
        account.signOut()

        await session.start()
        await session.createVault()
        let ceremony = try #require(session.recoveryCeremony)
        await ceremony.start()

        #expect(
            ceremony.phase == .failed(SessionModel.RecoverySetupError.signedOut.localizedDescription),
            "no bearer: the user signed out, and the ceremony heard"
        )
        #expect(account.accessToken == nil)
        #expect(store.stored != nil, "even though the credential is still in the Keychain")
        await session.lock()
    }

    /// The other half: the ceremony still takes the first look when nothing
    /// has read the account yet — a first run, where the sheet can start
    /// before the shell's own `.task` has restored anything.
    @Test
    func theCeremonyTakesTheFirstLookOnAFreshAccount() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let account = model(store: StubCredentialStore(value: credentials(accessToken: "access-old")))
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(),
            appVersion: "test",
            settingsDefaults: try configuredDefaults(),
            account: account
        )

        await session.start()
        await session.createVault()
        _ = try #require(session.recoveryCeremony)

        #expect(account.accessToken == "access-old", "the ceremony restored the session's own model")
        await session.lock()
    }

    /// Settings entered after launch — the Settings screen writes through its
    /// own ``AppSettings`` — reach the ceremony built afterwards. With no
    /// stored credential the ceremony then stops at the bearer, not at the
    /// configuration it would have seen had it kept the launch-time snapshot.
    @Test
    func theCeremonyReadsSettingsEnteredAfterLaunch() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let defaults = try scratchDefaults()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(),
            appVersion: "test",
            settingsDefaults: defaults,
            account: model(store: StubCredentialStore())
        )
        await session.start()
        configure(defaults)

        await session.createVault()
        let ceremony = try #require(session.recoveryCeremony)
        await ceremony.start()

        #expect(
            ceremony.phase == .failed(SessionModel.RecoverySetupError.signedOut.localizedDescription),
            "past the settings guard: the relay and email entered after launch were read"
        )
        await session.lock()
    }
}
