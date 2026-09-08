import Foundation
import Testing

@testable import Sunrise

/// Sync status is the one badge that can lie, and `Degraded` is the lie that
/// costs data. These tests are about that.
struct SyncPresentationTests {
    private func snapshot(
        _ state: SyncState,
        pending: UInt32 = 0
    ) -> SyncSnapshot {
        SyncSnapshot(state: state, outboxPending: pending, peerDevices: 1, lastSyncMs: nil)
    }

    /// The falsifier for this whole file: if someone ever folds `Degraded`
    /// into the connected-and-fine case, this fails.
    @Test
    func degradedNeverReadsAsSynced() {
        let degraded = SyncPresentation(snapshot(.degraded))
        let live = SyncPresentation(snapshot(.live))

        #expect(degraded.label != live.label)
        #expect(degraded.tone == .alert)
        #expect(degraded.isKnownIncomplete)
        #expect(degraded.detail?.isEmpty == false, "a user must be told what is missing")
    }

    /// Four states, four badges. A collision here means two different
    /// conditions look identical on screen.
    @Test
    func everyStateIsDistinguishable() {
        let all: [SyncState] = [.disconnected, .catchingUp, .live, .degraded]
        let labels = all.map { SyncPresentation(snapshot($0)).label }
        #expect(Set(labels).count == all.count, "two states share a label: \(labels)")

        let symbols = all.map { SyncPresentation(snapshot($0)).symbol }
        #expect(Set(symbols).count == all.count, "two states share an icon: \(symbols)")
    }

    /// Being offline is not being degraded. An offline vault is complete as
    /// far as it knows and catches up on reconnect; a degraded one has already
    /// been told it will not.
    @Test
    func onlyDegradedIsKnownIncomplete() {
        #expect(!SyncPresentation(snapshot(.disconnected)).isKnownIncomplete)
        #expect(!SyncPresentation(snapshot(.catchingUp)).isKnownIncomplete)
        #expect(!SyncPresentation(snapshot(.live)).isKnownIncomplete)
        #expect(SyncPresentation(snapshot(.degraded)).isKnownIncomplete)
    }

    /// Connected with an unsent backlog is not "Synced" either.
    @Test
    func aBacklogIsVisibleWhileConnected() {
        let idle = SyncPresentation(snapshot(.live))
        #expect(idle.label == "Synced")
        #expect(idle.detail == nil)

        let busy = SyncPresentation(snapshot(.live, pending: 3))
        #expect(busy.label != "Synced")
        #expect(busy.detail == "3 changes waiting to send")
        #expect(busy.tone == .working)
    }

    @Test
    func offlineSaysTheDataIsStillHere() {
        let offline = SyncPresentation(snapshot(.disconnected))
        #expect(offline.tone == .idle, "offline is a supported way to work, not a fault")
        #expect(offline.detail?.contains("up to date here") == true)

        let backlog = SyncPresentation(snapshot(.disconnected, pending: 1))
        #expect(backlog.detail == "1 change waiting to send")
    }

    /// Before the first answer the badge must not read as healthy.
    @Test
    func anUnansweredQueryIsNotSilence() {
        #expect(SyncPresentation.unknown.label == "Checking…")
        #expect(!SyncPresentation.unknown.isKnownIncomplete)
        #expect(SyncPresentation.unknown.label != SyncPresentation(snapshot(.live)).label)
    }
}

@MainActor
struct SyncStatusModelTests {
    @Test
    func aFreshModelReportsCheckingRatherThanHealthy() {
        let model = SyncStatusModel()
        #expect(model.snapshot == nil)
        #expect(model.presentation == .unknown)
    }

    /// An unsynced vault answers `Disconnected`, which is the honest reading
    /// of a core with no relay configured.
    @Test
    func aLocalOnlyVaultReadsAsOffline() async throws {
        let vault = try await TestVault()
        let model = SyncStatusModel()
        await model.refresh(from: vault.bridge)

        #expect(model.snapshot?.state == .disconnected)
        #expect(model.presentation.label == "Offline")
        await vault.bridge.shutdown()
    }
}

struct SyncPlanTests {
    @Test
    func noRelayMeansLocalOnly() {
        #expect(SyncPlan(relayURL: "", accessToken: "t") == .off(reason: "No relay is configured."))
        #expect(SyncPlan(relayURL: "   ", accessToken: nil) == .off(reason: "No relay is configured."))
    }

    /// A self-host relay accepts an unauthenticated upgrade. Refusing to
    /// connect without a token would break the only configuration that works
    /// with no identity provider at all.
    @Test
    func aSelfHostRelayConnectsWithNoBearer() {
        #expect(
            SyncPlan(relayURL: "http://127.0.0.1:8443", accessToken: nil)
                == .connect(url: "http://127.0.0.1:8443", bearer: nil, relayDeviceID: nil)
        )
    }

    /// An empty token is not a token. Passing it through produces a
    /// "malformed bearer" rejection instead of the anonymous request the user
    /// actually wanted.
    @Test
    func anEmptyTokenIsNoToken() {
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "  ")
                == .connect(url: "https://relay.example", bearer: nil, relayDeviceID: nil)
        )
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "tok")
                == .connect(url: "https://relay.example", bearer: "tok", relayDeviceID: nil)
        )
    }

    /// The device binding is carried, not invented. A relay device id reaches
    /// the plan from `SessionModel.relayDeviceID` and is passed straight
    /// through — the plan's job is to refuse the shapes that cannot work, and
    /// an id it cannot check is not one of them.
    @Test
    func aRegisteredDeviceCarriesItsRelayIDOntoTheConnection() {
        #expect(
            SyncPlan(
                relayURL: "https://relay.example",
                accessToken: "tok",
                relayDeviceID: "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y"
            ) == .connect(
                url: "https://relay.example",
                bearer: "tok",
                relayDeviceID: "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y"
            )
        )
    }

    /// An empty id is the empty-bearer trap one layer down, and worse: an
    /// `X-Sunrise-Device` naming no row is answered as a bad *bearer*, so the
    /// client cannot tell what it got wrong.
    @Test
    func anEmptyRelayDeviceIDIsNoBinding() {
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "tok", relayDeviceID: "  ")
                == .connect(url: "https://relay.example", bearer: "tok", relayDeviceID: nil)
        )
    }

    /// Not connecting without a binding would be the wrong refusal: the id
    /// only exists after registration, which happens over this same relay, and
    /// every self-host deployment runs without one on purpose.
    @Test
    func anUnregisteredDeviceStillConnects() {
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "tok")
                == .connect(url: "https://relay.example", bearer: "tok", relayDeviceID: nil)
        )
    }

    /// The scheme this test asserted was the *only* valid one until ADR-0023,
    /// and is now the only invalid one. Sync is an SSE stream over HTTP, so a
    /// relay is reached at its origin.
    @Test
    func aWebSocketURLIsRefusedWithAMigrationHint() {
        guard case let .off(reason) = SyncPlan(relayURL: "wss://relay.example/sync", accessToken: nil)
        else {
            Issue.record("a ws:// URL names nothing this app can reach")
            return
        }
        #expect(reason.contains("http://"))
        // The hint matters more than the refusal: a user with a working relay
        // URL from before the change needs to be told what to change it to,
        // not merely that it stopped working.
        #expect(reason.contains("WebSocket"))
    }
}
