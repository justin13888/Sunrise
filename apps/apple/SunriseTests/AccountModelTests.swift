import Foundation
import Security
import Testing

@testable import Sunrise

final class StubCredentialStore: CredentialStore, @unchecked Sendable {
    private let lock = NSLock()
    private var value: StoredCredentials?
    private var failure: (any Error)?
    private(set) var clearCount = 0

    init(value: StoredCredentials? = nil, loadError: (any Error)? = nil) {
        self.value = value
        failure = loadError
    }

    var stored: StoredCredentials? { lock.withLock { value } }

    /// The refusal `load` raises, settable mid-case: what a refused read costs
    /// is decided by the *next* look, so a case has to be able to unlock the
    /// keychain between two of them.
    var loadError: (any Error)? {
        get { lock.withLock { failure } }
        set { lock.withLock { failure = newValue } }
    }

    func load() throws -> StoredCredentials? {
        try lock.withLock {
            if let failure { throw failure }
            return value
        }
    }

    func save(_ credentials: StoredCredentials) throws { lock.withLock { value = credentials } }

    func clear() throws { lock.withLock { value = nil; clearCount += 1 } }
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
    /// The refusal the repair is about: the *other* keychain was reached and
    /// would not answer, so whether a token is in it is unknown.
    private static let refusal = KeychainError.otherDomainUnreadable(errSecInteractionNotAllowed)

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

    /// A store that *refuses* is not a store that answered nothing, and this
    /// is the difference the whole cross-domain read exists to keep. Reading a
    /// refusal as "signed out" offers Sign in, and signing in writes a second
    /// token beside the one that may still be sitting in the unreadable
    /// keychain — two secrets under one `(service, account)`, which is
    /// `migrationUnverified` on every later launch.
    @Test
    func aRefusedReadIsReportedInsteadOfLookingLikeASignedOutSession() {
        let store = StubCredentialStore(loadError: Self.refusal)
        let account = model(store: store)

        account.restore()

        #expect(account.state == .failed(Self.refusal.localizedDescription))
        #expect(account.state != .signedOut, "a refusal that renders as Sign in invites the second token")
        #expect(account.accessToken == nil)
    }

    /// And the Try again that state offers has to mean "look again". The view
    /// cannot know what the store answered, so the model is the only place the
    /// second token can be stopped: a store that has started answering hands
    /// back the session it was holding, without a browser and without writing
    /// over it.
    @Test
    func tryingAgainAfterARefusalLooksAgainRatherThanSigningInAfresh() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            loadError: Self.refusal
        )
        let opened = OpenedURLs()
        let account = model(store: store, opened: { opened.record($0) })
        account.restore()
        #expect(account.state == .failed(Self.refusal.localizedDescription))

        store.loadError = nil
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.accessToken == "access-old")
        #expect(store.stored?.accessToken == "access-old", "a second token was written over the first")
        #expect(opened.first == nil, "the refusal was spent on a login rather than a second look")
    }

    /// The other half of the same guard: the refusal is a reason to look
    /// again, not a latch. Once the store answers and there is genuinely
    /// nothing in it, signing in is safe and has to happen.
    @Test
    func aStoreThatAnswersNothingAfterARefusalStillReachesTheLogin() async {
        let store = StubCredentialStore(loadError: Self.refusal)
        let account = model(store: store)
        account.restore()

        store.loadError = nil
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(store.stored?.accessToken == "access-1")
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
