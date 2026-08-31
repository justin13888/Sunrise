import Foundation
import Testing

@testable import Sunrise

final class StubCredentialStore: CredentialStore, @unchecked Sendable {
    private let lock = NSLock()
    private var value: StoredCredentials?
    private(set) var clearCount = 0

    init(value: StoredCredentials? = nil) { self.value = value }

    var stored: StoredCredentials? { lock.withLock { value } }

    func load() throws -> StoredCredentials? { lock.withLock { value } }
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
