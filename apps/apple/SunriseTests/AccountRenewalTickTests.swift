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
            sleep: { try await clock.sleep($0) }
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
    /// 4 500 finds it expired and leaves `.failed` for the Account screen.
    @Test
    func aRenewalThatKeepsFailingSurfacesOnceTheTokenExpires() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let driver = StubLoginDriver(failure: StubLoginError())
        let account = model(store: store, driver: driver)
        account.restore()
        let clock = FakeRenewalClock(startMs: 3_500, stopAfter: 1)

        await run(account, on: clock)

        #expect(account.accessToken == nil)
        #expect(store.stored == nil)
        #expect(account.state == .failed("the issuer refused"))
    }

    /// The shells run the tick from a `.task`, and SwiftUI ends one by
    /// cancelling it. The default sleep is a real `Task.sleep`, so a cancel
    /// has to end the loop rather than leave it looking forever.
    @Test
    func cancellingTheTaskEndsTheTick() async {
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let account = model(store: store, driver: StubLoginDriver())
        account.restore()

        let tick = Task {
            await account.renewWhileRunning(
                issuer: { "https://issuer.example" },
                clientID: { "client" },
                now: { 0 },
                every: .seconds(3_600)
            )
        }
        tick.cancel()
        await tick.value

        #expect(account.accessToken == "access-old")
    }
}
