import Foundation
import Testing

@testable import Sunrise

/// A scratch vault, deleted when the test that made it goes away.
struct TestVault: ~Copyable {
    let directory: URL
    let bridge: CoreBridge

    init() async throws {
        directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
        bridge = try await CoreBridge.open(
            directory: directory,
            vaultRoot: Data(repeating: 42, count: 32),
            appVersion: "test"
        )
    }

    deinit {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// The bridge against a real vault. These are slower than the pure tests and
/// they earn it: an FFI seam fails at the boundary, and only a real call
/// crosses it.
struct CoreBridgeTests {
    @Test
    func aVaultOpensAndAnswersQueries() async throws {
        let vault = try await TestVault()
        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        #expect(outcome.entity.hasPrefix("tsk_"))

        let result = try await vault.bridge.query(.inbox)
        guard case let .tasks(tasks) = result else {
            Issue.record("expected a task list, got \(result)")
            return
        }
        #expect(tasks.map(\.title) == ["Renew passport"])
        await vault.bridge.shutdown()
    }

    /// A wrong-sized root is a typed error, not a trap. UniFFI turns a Rust
    /// panic into an unrecoverable `rustPanic` on this side, so anything the
    /// seam can reject it must reject.
    @Test
    func aBadVaultRootThrowsRatherThanTraps() async {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }

        await #expect(throws: BindingError.self) {
            _ = try await CoreBridge.open(
                directory: directory,
                vaultRoot: Data(repeating: 1, count: 8),
                appVersion: "test"
            )
        }
    }

    /// The subscription is live: a write on the bridge reaches the stream.
    @Test
    func aWriteReachesTheChangeStream() async throws {
        let vault = try await TestVault()
        let changes = await vault.bridge.changes(window: .milliseconds(20))

        let observed = Task { () -> ChangeBatch? in
            for await batch in changes { return batch }
            return nil
        }

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Book the ferry")))
        let batch = try #require(await observed.value)
        #expect(batch.touched.contains(outcome.entity))
        #expect(batch.isComplete)
        await vault.bridge.shutdown()
    }

    /// **The regression test for the feed the whole app shares.**
    ///
    /// Every screen model calls `changes()`. This method used to cancel the
    /// previous subscription, so the last one to start owned the feed and every
    /// other screen silently stopped repainting — on sync, on another device's
    /// writes, and on writes made elsewhere in this app. Against that version
    /// `alpha` below sees nothing at all.
    @Test
    func twoScreensBothFollowTheSameVault() async throws {
        let vault = try await TestVault()
        let alpha = await vault.bridge.changes(window: .milliseconds(20))
        let beta = await vault.bridge.changes(window: .milliseconds(20))

        let alphaSaw = Task { await Self.touched(in: alpha) }
        let betaSaw = Task { await Self.touched(in: beta) }

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Book the ferry")))
        try? await Task.sleep(for: .milliseconds(150))
        // Ends both streams, so the collectors above return.
        await vault.bridge.shutdown()

        #expect(await alphaSaw.value.contains(outcome.entity))
        #expect(
            await betaSaw.value.contains(outcome.entity),
            "the second screen to subscribe did not receive the write"
        )
    }

    /// One screen disappearing must not take the feed with it. The old bridge
    /// could not get this wrong because it only ever had one consumer; the new
    /// one has to be told to.
    @Test
    func oneScreenGoingAwayLeavesTheOtherFollowing() async throws {
        let vault = try await TestVault()
        let leaving = await vault.bridge.changes(window: .milliseconds(20))
        let staying = await vault.bridge.changes(window: .milliseconds(20))

        let stayingSaw = Task { await Self.touched(in: staying) }
        // A view that appears, repaints once and disappears.
        let departed = Task { () -> ChangeBatch? in
            for await batch in leaving { return batch }
            return nil
        }

        let first = try await vault.bridge.submit(.createTask(draft: draft("Pack the tent")))
        #expect(await departed.value != nil)

        let second = try await vault.bridge.submit(.createTask(draft: draft("Find the pegs")))
        try? await Task.sleep(for: .milliseconds(150))
        await vault.bridge.shutdown()

        let touched = await stayingSaw.value
        #expect(touched.contains(first.entity))
        #expect(
            touched.contains(second.entity),
            "the surviving screen stopped repainting when its neighbour went away"
        )
    }

    /// Everything a stream reported, across however many batches it took. The
    /// coalescing window makes the split between batches a timing detail, and
    /// a test that asserted on it would be flaky rather than strict.
    private static func touched(in stream: AsyncStream<ChangeBatch>) async -> Set<EntityRef> {
        var seen: Set<EntityRef> = []
        for await batch in stream { seen.formUnion(batch.touched) }
        return seen
    }

    /// Preview is a read. A capture field re-parses on every keystroke, and a
    /// version of this that wrote would create one task per character.
    @Test
    func previewingACaptureWritesNothing() async throws {
        let vault = try await TestVault()
        let preview = try await vault.bridge.previewCapture("Renew passport !1 ~1h", timeZone: "UTC")
        #expect(preview.draft.title == "Renew passport")
        #expect(preview.draft.priority == 1)
        #expect(preview.draft.estimatedDurationS == 3600)

        let result = try await vault.bridge.query(.inbox)
        guard case let .tasks(tasks) = result else {
            Issue.record("expected a task list, got \(result)")
            return
        }
        #expect(tasks.isEmpty)
        await vault.bridge.shutdown()
    }

    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }
}
