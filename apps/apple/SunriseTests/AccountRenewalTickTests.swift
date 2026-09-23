import Foundation
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

    /// The shells run the tick from a `.task`, and SwiftUI ends one by
    /// cancelling it. The default sleep is a real `Task.sleep`, so a cancel
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

    init(failures: Int, renewed: StoredCredentials, completed: StoredCredentials? = nil) {
        failuresLeft = failures
        self.renewed = renewed
        self.completed = completed ?? renewed
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
        if fails { throw StubLoginError() }
        return renewed
    }
}
