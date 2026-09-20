import Foundation
import Security
import Testing

@testable import Sunrise

final class StubCredentialStore: CredentialStore, @unchecked Sendable {
    private let lock = NSLock()
    private var value: StoredCredentials?
    private(set) var clearCount = 0
    /// What `clear()` refuses with, standing in for a locked Keychain. The
    /// value survives the refusal, which is the fact this is about. Mutable
    /// because a Keychain unlocks: refused, then unlocked, then a second
    /// sign-out is what the disclosure's own button drives, and a refusal
    /// fixed at construction cannot express it.
    private var clearFailure: (any Error)?
    /// What `save()` refuses with — the lock that refuses a `clear()` refuses
    /// a `save()` too, so a sign-in under the warning establishes nothing.
    private var saveFailure: (any Error)?

    init(value: StoredCredentials? = nil, clearFailure: (any Error)? = nil) {
        self.value = value
        self.clearFailure = clearFailure
    }

    var stored: StoredCredentials? { lock.withLock { value } }

    /// The user unlocked the Keychain.
    func stopRefusingClears() { lock.withLock { clearFailure = nil } }
    func refuseSaves(with error: any Error) { lock.withLock { saveFailure = error } }

    func load() throws -> StoredCredentials? { lock.withLock { value } }

    func save(_ credentials: StoredCredentials) throws {
        try lock.withLock {
            if let saveFailure { throw saveFailure }
            value = credentials
        }
    }

    /// `clearCount` counts attempts, so it still reads 1 after a refusal.
    func clear() throws {
        try lock.withLock {
            clearCount += 1
            if let clearFailure { throw clearFailure }
            value = nil
        }
    }
}

struct StubLoginDriver: LoginDriver {
    var authorizeURL = URL(string: "https://issuer.example/authorize")!
    var completed: StoredCredentials?
    var refreshed: StoredCredentials?
    var failure: (any Error)?

    func begin(deviceID: String) async throws -> URL {
        if let failure { throw failure }
        return authorizeURL.appending(queryItems: [.init(name: "device_id", value: deviceID)])
    }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        if let failure { throw failure }
        return completed ?? credentials(accessToken: "access-1")
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        if let failure { throw failure }
        guard let refreshed else { throw StubLoginError() }
        return refreshed
    }
}

struct StubLoginError: Error, LocalizedError {
    let errorDescription: String? = "the issuer refused"
}

func credentials(
    accessToken: String,
    refreshToken: String? = "refresh-1",
    expiresAtMs: UInt64 = 4_000,
    renewAtMs: UInt64 = 3_000
) -> StoredCredentials {
    StoredCredentials(
        accessToken: accessToken,
        refreshToken: refreshToken,
        expiresAtMs: expiresAtMs,
        renewAtMs: renewAtMs
    )
}

@MainActor
struct AccountModelTests {
    private func model(
        store: StubCredentialStore,
        driver: StubLoginDriver = StubLoginDriver(),
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(
            store: store,
            makeDriver: { _, _ in driver },
            openURL: opened ?? { _ in }
        )
    }

    @Test
    func aCompletedLoginIsStoredAndBecomesTheBearer() async {
        let store = StubCredentialStore()
        let account = model(store: store)

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.accessToken == "access-1")
        #expect(store.stored?.accessToken == "access-1")
    }

    /// The device binding is the point of the flow, not decoration: a token
    /// whose claim names another device is refused by the relay.
    @Test
    func theAuthorizeURLCarriesTheDeviceID() async {
        let opened = OpenedURLs()
        let account = model(store: StubCredentialStore(), opened: { opened.record($0) })

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "device-42",
            nowMs: 0
        )

        #expect(opened.first?.query?.contains("device_id=device-42") == true)
    }

    @Test
    func anUnconfiguredIssuerIsRefusedBeforeAnyNetworkCall() async {
        let store = StubCredentialStore()
        let account = model(store: store)

        await account.signIn(issuer: "  ", clientID: "client", deviceID: "abcd", nowMs: 0)

        #expect(account.state == .failed(AccountError.notConfigured.localizedDescription))
        #expect(store.stored == nil)
    }

    @Test
    func aFailedLoginLeavesNoHalfSession() async {
        let store = StubCredentialStore()
        let driver = StubLoginDriver(failure: StubLoginError())
        let account = model(store: store, driver: driver)

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .failed("the issuer refused"))
        #expect(account.accessToken == nil)
        #expect(store.stored == nil)
    }

    @Test
    func aStoredTokenIsRestoredWithoutOpeningABrowser() {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let opened = OpenedURLs()
        let account = model(store: store, opened: { opened.record($0) })

        account.restore()

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.accessToken == "access-old")
        #expect(opened.first == nil)
    }

    @Test
    func renewalIsSilentAndOnlyHappensAtTheRenewalPoint() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        var driver = StubLoginDriver()
        driver.refreshed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, driver: driver)
        account.restore()

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 2_999)
        #expect(account.accessToken == "access-old", "renewed before it was due")

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_000)
        #expect(account.accessToken == "access-new")
        #expect(store.stored?.accessToken == "access-new")
    }

    /// A network blink at the renewal point must not sign the user out: the
    /// current token is good until it expires, and the next tick tries again.
    @Test
    func aFailedRenewalKeepsTheStillValidToken() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = StubLoginDriver(failure: StubLoginError())
        let account = model(store: store, driver: driver)
        account.restore()

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_500)

        #expect(account.accessToken == "access-old")
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
    }

    @Test
    func anExpiredTokenThatCannotBeRenewedSignsOut() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = StubLoginDriver(failure: StubLoginError())
        let account = model(store: store, driver: driver)
        account.restore()

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 4_001)

        #expect(account.accessToken == nil)
        #expect(store.stored == nil)
        #expect(
            account.state == .failed("the issuer refused"),
            "why the session ended was overwritten by the sign-out that followed it"
        )
    }

    @Test
    func signingOutClearsTheKeychainNotJustTheModel() {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store)
        account.restore()

        account.signOut()

        #expect(account.state == .signedOut)
        #expect(store.stored == nil)
        #expect(store.clearCount == 1)
    }

    /// The defect this pins: `try? store.clear()` presented `.signedOut` over a
    /// refusal, the token survived in the Keychain, and `restore()` signed the
    /// user back in on the next launch without a word. What is disclosed is
    /// the Keychain's own prose, because the user is being told to go and
    /// unlock something.
    @Test
    func aRefusedClearIsDisclosedRatherThanSwallowed() {
        let refusal = KeychainError.unexpected(errSecInteractionNotAllowed)
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: refusal
        )
        let account = model(store: store)
        account.restore()

        account.signOut()

        #expect(account.state == .signedOut, "the local session ends regardless")
        #expect(account.accessToken == nil)
        #expect(store.stored != nil, "the token is still there — that is what is disclosed")
        #expect(
            account.signOutIncomplete == refusal.localizedDescription,
            "the Keychain's message, not `String(describing:)`, which says `unexpected(-25308)`"
        )
        #expect(store.clearCount == 1, "the attempt is counted even though it was refused")
    }

    /// The harm itself, driven end to end: the refusal leaves the credential
    /// in the Keychain, and the very next `restore()` — which runs from the
    /// window and scene `.task` blocks, so on the next launch — reads it back
    /// and signs the user into the session they deliberately ended. This is
    /// the sequence the disclosure warns about; nothing else in the suite
    /// executes it.
    @Test
    func theSurvivingCredentialSignsTheUserBackInOnTheNextRestore() {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: KeychainError.unexpected(errSecInteractionNotAllowed)
        )
        let account = model(store: store)
        account.restore()

        account.signOut()
        #expect(account.state == .signedOut)
        #expect(account.accessToken == nil)

        account.restore()

        #expect(
            account.state == .signedIn(expiresAtMs: 4_000),
            "the session the user ended is back — this is #255"
        )
        #expect(
            account.accessToken == "access-old",
            "and it is the same bearer, not a fresh one"
        )
        #expect(
            account.signOutIncomplete != nil,
            "the warning outlives the restore, so the view can still disclose it"
        )
    }

    @Test
    func aSignOutThatWorkedDisclosesNothing() {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store)
        account.restore()

        account.signOut()

        #expect(account.signOutIncomplete == nil)
    }

    /// Dismissing says the user has read it, and a later sign-in clears it on
    /// its own — neither leaves a stale warning under a live session.
    @Test
    func theDisclosureIsDismissedAndDoesNotOutliveTheNextSignIn() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: KeychainError.unexpected(errSecInteractionNotAllowed)
        )
        let account = model(store: store)
        account.signOut()
        #expect(account.signOutIncomplete != nil)

        account.dismissSignOutIncomplete()
        #expect(account.signOutIncomplete == nil)

        account.signOut()
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.signOutIncomplete == nil)
    }

    /// A sign-in that never established a session has not replaced the
    /// credential the disclosure is about, so the disclosure is still true.
    /// Clearing it on entry to `signIn()` destroyed it on both of these paths
    /// and left the user with a stored refresh token and no indication of it.
    @Test
    func aSignInRefusedBeforeItStartedLeavesTheWarningStanding() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: KeychainError.unexpected(errSecInteractionNotAllowed)
        )
        let account = model(store: store)
        account.signOut()
        #expect(account.signOutIncomplete != nil)

        await account.signIn(issuer: "  ", clientID: "", deviceID: "abcd", nowMs: 0)

        #expect(account.state == .failed(AccountError.notConfigured.localizedDescription))
        #expect(store.stored != nil, "the credential the warning is about is still stored")
        #expect(
            account.signOutIncomplete != nil,
            "a sign-in that never started leaves the stored credential, and the warning about it"
        )
    }

    @Test
    func aSignInThatFailedLeavesTheWarningStanding() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: KeychainError.unexpected(errSecInteractionNotAllowed)
        )
        let account = model(store: store, driver: StubLoginDriver(failure: StubLoginError()))
        account.signOut()
        #expect(account.signOutIncomplete != nil)

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .failed("the issuer refused"))
        #expect(store.stored != nil, "the credential the warning is about is still stored")
        #expect(
            account.signOutIncomplete != nil,
            "the issuer refusing does not make the surviving refresh token go away"
        )
    }

    /// The lock that refused the `clear()` refuses the `save()`, so a sign-in
    /// attempted under the warning establishes nothing and the credential the
    /// warning is about is still the stored one. Clearing the warning before
    /// `store.save` rather than after would destroy it on exactly this path.
    @Test
    func aSignInThatCannotSaveLeavesTheWarningAndNoSession() async {
        let refusal = KeychainError.unexpected(errSecInteractionNotAllowed)
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: refusal
        )
        let account = model(store: store)
        account.signOut()
        store.refuseSaves(with: refusal)

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.accessToken == nil, "a save that failed is not a session")
        #expect(account.state == .failed(refusal.localizedDescription))
        #expect(store.stored?.accessToken == "access-old", "the old credential is still stored")
        #expect(account.signOutIncomplete != nil, "so the warning about it is still true")
    }

    /// A struct carrying a live bearer and a refresh token ends up in the
    /// first log line anyone writes while debugging, unless it cannot.
    @Test
    func credentialsRedactThemselves() {
        let text = "\(credentials(accessToken: "super-secret-access", refreshToken: "super-secret-refresh"))"
        #expect(!text.contains("super-secret-access"))
        #expect(!text.contains("super-secret-refresh"))
        #expect(text.contains("expiresAtMs: 4000"))
    }
}

/// Records what the model asked the system to open.
final class OpenedURLs: @unchecked Sendable {
    private let lock = NSLock()
    private var urls: [URL] = []

    func record(_ url: URL) { lock.withLock { urls.append(url) } }
    var first: URL? { lock.withLock { urls.first } }
}
