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
                == .connect(url: "http://127.0.0.1:8443", bearer: nil)
        )
    }

    /// An empty token is not a token. Passing it through produces a
    /// "malformed bearer" rejection instead of the anonymous request the user
    /// actually wanted.
    @Test
    func anEmptyTokenIsNoToken() {
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "  ")
                == .connect(url: "https://relay.example", bearer: nil)
        )
        #expect(
            SyncPlan(relayURL: "https://relay.example", accessToken: "tok")
                == .connect(url: "https://relay.example", bearer: "tok")
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
