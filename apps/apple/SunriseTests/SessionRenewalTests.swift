import Foundation
import Testing

@testable import Sunrise

/// Who owns the renewal tick, and so how long it lives (#307).
///
/// A Mac window used to run it from a `.task`, and closing the window ended it
/// while the menu bar kept the vault — and its sync driver — open. These pin
/// the session as the owner: the tick outlives the task that started it and
/// ends only when the vault is let go of.
///
/// Opens a real vault in a scratch directory, as ``SessionModelTests`` does.
/// The helpers come from ``AccountModelTests``' file and ``StubRootStore``
/// from ``SessionModelTests``'.
@MainActor
struct SessionRenewalTests {
    /// Due at once against the core's wall clock; renewed into a token that
    /// is not due again for the life of the test.
    private func account() -> AccountModel {
        var stub = StubLoginDriver()
        stub.refreshed = credentials(accessToken: "access-new", expiresAtMs: 1 << 62, renewAtMs: 1 << 61)
        let driver = stub
        let account = AccountModel(
            store: StubCredentialStore(value: credentials(accessToken: "access-old")),
            makeDriver: { _, _ in driver },
            openURL: { _ in }
        )
        account.restoreIfUnread()
        return account
    }

    private func openSession(in directory: URL, account: AccountModel) async throws -> SessionModel {
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(),
            appVersion: "test",
            settingsDefaults: try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)")),
            account: account
        )
        await session.start()
        await session.createVault()
        try #require(session.phase == .unlocked)
        return session
    }

    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory.appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    /// **The defect.** The task that asked for renewal — a window's `.task` —
    /// ends, and the tick goes on looking: two more sleeps after it is gone.
    @Test
    func theTickOutlivesTheTaskThatStartedIt() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let account = account()
        let session = try await openSession(in: directory, account: account)
        let (asleep, sleeping) = AsyncStream.makeStream(of: Void.self)
        var looks = asleep.makeAsyncIterator()

        let window = _Concurrency.Task { @MainActor in
            session.renewSessionWhileOpen(every: .milliseconds(1)) { interval in
                sleeping.yield()
                try await _Concurrency.Task.sleep(for: interval)
            }
            try? await _Concurrency.Task.sleep(for: .seconds(3_600))
        }
        _ = await looks.next()
        window.cancel()
        await window.value
        _ = await looks.next()
        _ = await looks.next()

        #expect(session.isRenewingSession, "the window's task ended and the tick did not")
        #expect(account.accessToken == "access-new", "and its first look renewed")
        await session.lock()
    }

    /// Letting go of the vault is what ends it: the sleep it is parked in is
    /// cancelled. The vault opened next has no tick until a shell asks.
    @Test
    func lockingTheVaultEndsTheTick() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let session = try await openSession(in: directory, account: account())
        let (asleep, sleeping) = AsyncStream.makeStream(of: Void.self)
        let (ended, ending) = AsyncStream.makeStream(of: Void.self)

        session.renewSessionWhileOpen(every: .seconds(3_600)) { interval in
            sleeping.yield()
            do {
                try await _Concurrency.Task.sleep(for: interval)
            } catch {
                ending.yield()
                throw error
            }
        }
        for await _ in asleep { break }
        #expect(session.isRenewingSession)

        await session.lock()
        #expect(!session.isRenewingSession)
        for await _ in ended { break }

        await session.start()
        try #require(session.phase == .unlocked)
        #expect(!session.isRenewingSession, "a reopened vault has no tick of the closed one's")
        await session.lock()
    }

    /// Every window asks, and a second window, or one closed and reopened,
    /// must not start a second tick against the same vault.
    @Test
    func aSecondAskDoesNotStartASecondTick() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let session = try await openSession(in: directory, account: account())
        let bridge = try #require(session.bridge)

        session.renewSessionWhileOpen(every: .seconds(3_600))

        #expect(!session.renewal.start(against: bridge) {}, "one tick is already running")
        #expect(session.isRenewingSession)
        await session.lock()
    }

    /// With no vault open there is nothing to renew against, and asking starts
    /// nothing.
    @Test
    func noOpenVaultMeansNoTick() {
        let session = SessionModel(
            location: VaultLocation(directory: scratchDirectory()),
            rootStore: StubRootStore(),
            appVersion: "test",
            account: account()
        )

        session.renewSessionWhileOpen()

        #expect(!session.isRenewingSession)
    }
}
