import Foundation
import Security
import Testing

@testable import Sunrise

/// A clock that moves only when the loop sleeps, by exactly what it asked for,
/// and ends the loop after a fixed number of sleeps by throwing what a
/// cancelled `Task.sleep` throws.
@MainActor
final class FakeRenewalClock {
    private(set) var nowMs: UInt64
    private(set) var sleeps = 0
    private let stopAfter: Int

    init(startMs: UInt64, stopAfter: Int) {
        nowMs = startMs
        self.stopAfter = stopAfter
    }

    func sleep(_ interval: Duration) throws {
        guard sleeps < stopAfter else { throw CancellationError() }
        sleeps += 1
        nowMs += UInt64(interval.components.seconds) * 1_000
    }
}

/// The tick that makes ``AccountModel/refreshIfNeeded(issuer:clientID:nowMs:)``
/// run in the shipping app. Driven on ``FakeRenewalClock`` rather than wall
/// time, so each case says exactly which look renews.
@MainActor
struct AccountRenewalTickTests {
    private func model(store: StubCredentialStore, driver: StubLoginDriver) -> AccountModel {
        AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
    }

    private func run(_ account: AccountModel, on clock: FakeRenewalClock) async {
        await account.renewWhileRunning(
            issuer: { "https://issuer.example" },
            clientID: { "client" },
            now: { clock.nowMs },
            every: .seconds(1),
            sleep: { try clock.sleep($0) }
        )
    }

    /// Looks at 2 000 (not due), 3 000 (due: renews) and 4 000 (the renewed
    /// token is not due until 8 000), then stops.
    @Test
    func theTickRenewsASignedInSessionAtItsRenewalPoint() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        var driver = StubLoginDriver()
        driver.refreshed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, driver: driver)
        account.restore()
        let clock = FakeRenewalClock(startMs: 2_000, stopAfter: 2)

        await run(account, on: clock)

        #expect(clock.sleeps == 2, "the loop ended on the sleep that threw, not before")
        #expect(account.accessToken == "access-new")
        #expect(store.stored?.accessToken == "access-new")
        #expect(account.state == .signedIn(expiresAtMs: 9_000))
    }

    /// Each bearer a look changes reaches `tokenChanged` once, and a look that
    /// changed nothing reaches it not at all: the sync driver hears of the
    /// renewal at 3 000 and of nothing at 2 000 or 4 000.
    @Test
    func theTickHandsEachRenewedBearerOnOnce() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        var driver = StubLoginDriver()
        driver.refreshed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, driver: driver)
        account.restore()
        let clock = FakeRenewalClock(startMs: 2_000, stopAfter: 2)
        let handed = Observed<[String?]>()
        handed.value = []

        await account.renewWhileRunning(
            issuer: { "https://issuer.example" },
            clientID: { "client" },
            now: { clock.nowMs },
            every: .seconds(1),
            sleep: { try clock.sleep($0) },
            tokenChanged: { handed.value?.append($0) }
        )

        #expect(clock.sleeps == 2)
        #expect(handed.value == ["access-new"])
    }

    /// A renewal that lands after its tick was cancelled still publishes to the
    /// model, but reaches no `tokenChanged`: the owner that cancelled it has
    /// let go of the vault whose sync driver that callback names. The driver
    /// cancels the tick from inside `refresh`, so the cancel lands while
    /// ``AccountModel/refreshIfNeeded(issuer:clientID:nowMs:)`` is in flight
    /// with a changed token.
    @Test
    func aRenewalLandingAfterCancellationIsNotHandedOn() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = CancellingLoginDriver(
            renewed: credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        )
        let account = AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
        account.restore()
        let handed = Observed<[String?]>()
        handed.value = []

        let tick = _Concurrency.Task {
            await account.renewWhileRunning(
                issuer: { "https://issuer.example" },
                clientID: { "client" },
                now: { 3_000 },
                every: .seconds(3_600),
                tokenChanged: { handed.value?.append($0) }
            )
        }
        await tick.value

        #expect(account.accessToken == "access-new", "the renewal in flight landed")
        #expect(handed.value == [], "and was handed to nothing")
    }

    /// An expired renewal that fails drops the bearer, and the driver is told
    /// so rather than left presenting a token the relay refuses.
    @Test
    func aBearerDroppedOnExpiryIsHandedOnAsNil() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store, driver: StubLoginDriver(failure: StubLoginError()))
        account.restore()
        let clock = FakeRenewalClock(startMs: 4_500, stopAfter: 0)
        let handed = Observed<[String?]>()
        handed.value = []

        await account.renewWhileRunning(
            issuer: { "https://issuer.example" },
            clientID: { "client" },
            now: { clock.nowMs },
            sleep: { try clock.sleep($0) },
            tokenChanged: { handed.value?.append($0) }
        )

        #expect(handed.value == [nil])
    }

    /// The first look is before the first sleep: a restored token already
    /// past its renewal point is renewed at once, not an interval later.
    @Test
    func theFirstLookHappensBeforeTheFirstSleep() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        var driver = StubLoginDriver()
        driver.refreshed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, driver: driver)
        account.restore()
        let clock = FakeRenewalClock(startMs: 3_500, stopAfter: 0)

        await run(account, on: clock)

        #expect(clock.sleeps == 0)
        #expect(account.accessToken == "access-new")
    }

    /// A renewal that keeps failing is silent while the token is good and
    /// visible once it is not: the look at 3 500 keeps the token, the look at
    /// 4 500 finds it expired, drops the bearer and leaves `.failed` for the
    /// Account screen. The refresh token stays for the next look.
    @Test
    func aRenewalThatKeepsFailingSurfacesOnceTheTokenExpires() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = StubLoginDriver(failure: StubLoginError())
        let account = model(store: store, driver: driver)
        account.restore()
        let clock = FakeRenewalClock(startMs: 3_500, stopAfter: 1)

        await run(account, on: clock)

        #expect(account.accessToken == nil)
        #expect(store.stored?.refreshToken == "refresh-1")
        #expect(store.clearCount == 0)
        #expect(account.state == .failed("the issuer refused"))
    }

    /// An offline launch, or a Mac waking before its network: the looks at
    /// 3 500 and 4 500 fail, the second one after expiry, and the look at
    /// 5 500 renews with the refresh token that failure kept. No browser.
    @Test
    func aSessionThatExpiredOfflineRenewsOnceTheNetworkIsBack() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = ScriptedLoginDriver(
            failures: 2,
            renewed: credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        )
        let account = AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
        account.restore()
        let clock = FakeRenewalClock(startMs: 3_500, stopAfter: 2)

        await run(account, on: clock)

        #expect(driver.refreshCount == 3)
        #expect(account.accessToken == "access-new")
        #expect(store.stored?.accessToken == "access-new")
        #expect(store.clearCount == 0, "nothing ever deleted the credential")
        #expect(account.state == .signedIn(expiresAtMs: 9_000))
    }

    /// The tick keeps looking after an expired renewal fails, so it can fail
    /// again while **Try again** has a login parked in the browser. That
    /// failure must not replace the spinner with an error while the browser
    /// is still open.
    @Test
    func aRenewalFailingDuringALoginLeavesItsSpinner() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = ScriptedLoginDriver(
            failures: .max,
            renewed: credentials(accessToken: "unused"),
            completed: credentials(accessToken: "access-login", expiresAtMs: 9_000, renewAtMs: 8_000)
        )
        let account = AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
        account.restore()
        let seen = Observed<AccountModel.State>()
        driver.setWhileParked {
            await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 4_500)
            seen.value = account.state
        }

        await account.signIn(issuer: "https://issuer.example", clientID: "c", deviceID: "d", nowMs: 4_500)

        #expect(driver.refreshCount == 1, "the renewal ran while the browser was open")
        #expect(seen.value == .awaitingBrowser)
        #expect(account.state == .signedIn(expiresAtMs: 9_000))
        #expect(account.accessToken == "access-login")
    }

    /// What the issuer says about a refresh token it has revoked or expired.
    private static let refusal = BindingError.LoginRefused(
        message: "login: issuer declined: refresh: invalid_grant: Token is not active"
    )

    /// A refused refresh token ends the session at the first look, while the
    /// bearer is still good (3 500 < 4 000), and the tick stops asking: the
    /// look at 4 500 finds nothing to renew. Keeping it asked the issuer again
    /// every thirty seconds until the user signed in or out.
    @Test
    func aRefusedRenewalEndsTheSessionWhileTheTokenIsStillValid() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = ScriptedLoginDriver(
            failures: .max,
            renewed: credentials(accessToken: "unused"),
            failWith: Self.refusal
        )
        let account = AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
        account.restore()
        let clock = FakeRenewalClock(startMs: 3_500, stopAfter: 1)
        let handed = Observed<[String?]>()
        handed.value = []

        await account.renewWhileRunning(
            issuer: { "https://issuer.example" },
            clientID: { "client" },
            now: { clock.nowMs },
            every: .seconds(1),
            sleep: { try clock.sleep($0) },
            tokenChanged: { handed.value?.append($0) }
        )

        #expect(driver.refreshCount == 1, "the refused token was offered to the issuer again")
        #expect(store.stored == nil)
        #expect(store.clearCount == 1)
        #expect(account.accessToken == nil)
        #expect(handed.value == [nil], "the sync driver heard the bearer go")
        #expect(account.state == .failed(AccountError.refusedByIssuer.localizedDescription))
        #expect(!account.offersBareSignOut, "nothing is left for a bare Sign out to remove")
    }

    /// The refusal goes through ``AccountModel/signOut()``, so a Keychain that
    /// will not let go of the refused token is disclosed, not swallowed.
    @Test
    func aRefusedRenewalWhoseClearIsRefusedSaysSo() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            clearFailure: KeychainError.unexpected(errSecInteractionNotAllowed)
        )
        let account = model(store: store, driver: StubLoginDriver(failure: Self.refusal))
        account.restore()

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_500)

        #expect(store.stored?.refreshToken == "refresh-1", "the stub keeps what a refused clear keeps")
        #expect(account.accessToken == nil)
        #expect(account.signOutResidue != .none)
        #expect(account.state == .failed(AccountError.refusedByIssuer.localizedDescription))
    }

    /// A refusal that lands while **Try again** has a login in the browser
    /// leaves that login alone: it is about to replace the refused token, and
    /// a sign-out here would discard it on its way back.
    @Test
    func aRefusalDuringALoginLeavesTheLoginToFinish() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = ScriptedLoginDriver(
            failures: .max,
            renewed: credentials(accessToken: "unused"),
            completed: credentials(accessToken: "access-login", expiresAtMs: 9_000, renewAtMs: 8_000),
            failWith: Self.refusal
        )
        let account = AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: { _ in })
        account.restore()
        let seen = Observed<AccountModel.State>()
        driver.setWhileParked {
            await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_500)
            seen.value = account.state
        }

        await account.signIn(issuer: "https://issuer.example", clientID: "c", deviceID: "d", nowMs: 3_500)

        #expect(driver.refreshCount == 1, "the renewal ran while the browser was open")
        #expect(seen.value == .awaitingBrowser)
        #expect(store.clearCount == 0)
        #expect(account.state == .signedIn(expiresAtMs: 9_000))
        #expect(store.stored?.accessToken == "access-login")
    }

    /// The session runs the tick in a task it holds, and ends it by
    /// cancelling that task (#307). The default sleep is a real `Task.sleep`, so a cancel
    /// that arrives while the loop is asleep has to end the loop rather than
    /// leave it looking forever. The cancel waits until the loop has entered
    /// its sleep, so the loop's own `isCancelled` check cannot be what ends it.
    @Test
    func cancellingTheTaskDuringItsSleepEndsTheTick() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store, driver: StubLoginDriver())
        account.restore()
        let (asleep, sleeping) = AsyncStream.makeStream(of: Void.self)
        let sleepThrew = Observed<Bool>()

        let tick = _Concurrency.Task {
            await account.renewWhileRunning(
                issuer: { "https://issuer.example" },
                clientID: { "client" },
                now: { 0 },
                every: .seconds(3_600),
                sleep: { interval in
                    sleeping.yield()
                    do {
                        try await _Concurrency.Task.sleep(for: interval)
                    } catch {
                        sleepThrew.value = true
                        throw error
                    }
                }
            )
        }
        for await _ in asleep { break }
        tick.cancel()
        await tick.value

        #expect(sleepThrew.value == true, "the loop ended through the real sleep's cancellation")
        #expect(account.accessToken == "access-old")
    }
}

/// Something a closure saw, read back by the test that made the closure.
@MainActor
final class Observed<Value> {
    var value: Value?
}

/// Renewals that fail `failures` times and then succeed, and a login whose
/// `complete` first runs a hook, so a test can act while it is parked in the
/// browser. `StubLoginDriver`'s one `failure` knob can express neither.
final class ScriptedLoginDriver: LoginDriver, @unchecked Sendable {
    private let lock = NSLock()
    private var failuresLeft: Int
    private var refreshes = 0
    private var whileParked: (@MainActor @Sendable () async -> Void)?
    private let renewed: StoredCredentials
    private let completed: StoredCredentials
    /// What each failing renewal throws: a network-shaped failure by default,
    /// or the issuer's refusal.
    private let failure: any Error

    init(
        failures: Int,
        renewed: StoredCredentials,
        completed: StoredCredentials? = nil,
        failWith failure: any Error = StubLoginError()
    ) {
        failuresLeft = failures
        self.renewed = renewed
        self.completed = completed ?? renewed
        self.failure = failure
    }

    var refreshCount: Int { lock.withLock { refreshes } }

    func setWhileParked(_ hook: @escaping @MainActor @Sendable () async -> Void) {
        lock.withLock { whileParked = hook }
    }

    func begin(deviceID: String) async throws -> URL { StubLoginDriver().authorizeURL }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        if let hook = lock.withLock({ whileParked }) { await hook() }
        return completed
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        let fails = lock.withLock {
            refreshes += 1
            guard failuresLeft > 0 else { return false }
            failuresLeft -= 1
            return true
        }
        if fails { throw failure }
        return renewed
    }
}

/// A renewal that cancels the task running it, then succeeds: the cancel lands
/// while the renewal is in flight, as a session letting go of its vault would
/// mid-refresh, with no timing for the test to arrange.
struct CancellingLoginDriver: LoginDriver {
    let renewed: StoredCredentials

    func begin(deviceID: String) async throws -> URL { StubLoginDriver().authorizeURL }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials { renewed }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        withUnsafeCurrentTask { $0?.cancel() }
        return renewed
    }
}
