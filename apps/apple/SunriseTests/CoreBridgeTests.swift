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

        // Past the prime: `changes()` opens with an empty batch that means
        // "re-read", and the claim here is about a write actually being
        // announced. Bounded, because a write that is never announced is
        // silence, and an unbounded wait would hang the suite instead of
        // failing it.
        let observed = Task {
            await Self.firstBatch(of: changes, matching: { !$0.touched.isEmpty })
        }

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Book the ferry")))
        let batch = try #require(
            await observed.value,
            "the write was never announced on the stream"
        )
        #expect(batch.touched.contains(outcome.entity))
        #expect(batch.isComplete)
        await vault.bridge.shutdown()
    }

    /// **A feed that is opened late is told to re-read.**
    ///
    /// `changes()` subscribes on its first caller, so there is always a window
    /// between a write landing and a screen attaching — at launch, on a tab
    /// the user has not opened yet, and in any test that starts `follow()` as
    /// a task and writes immediately. `Core::changes()` is a `tokio`
    /// broadcast: a receiver created after a send does not get it, and no
    /// timeout makes a missed event arrive. The prime is what covers that
    /// window, and this is the interleaving it exists for — the write is
    /// complete before anything subscribes.
    ///
    /// So what arrives says nothing about the missed write, and never will:
    /// the batch below is empty and incomplete, which is an instruction to
    /// re-read rather than a report. The entity is recovered by the query that
    /// instruction provokes, which is the second half of this test. `primed()`
    /// puts it the same way — "a missed event does not arrive late, it does
    /// not arrive".
    @Test
    func aStreamOpenedAfterAWriteStillAsksForARepaint() async throws {
        let vault = try await TestVault()
        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Already here")))

        let changes = await vault.bridge.changes(window: .milliseconds(20))
        // Bounded, because the way this fails is that nothing ever arrives:
        // an unbounded wait would hang the suite instead of failing it.
        let batch = try #require(
            await Self.firstBatch(of: changes),
            "a stream opened after the write said nothing at all"
        )
        #expect(
            batch.touched.isEmpty && !batch.isComplete,
            "a late subscriber is told to re-read rather than told nothing"
        )
        // And the re-read it is being told to do finds the write.
        guard case let .tasks(tasks) = try await vault.bridge.query(.inbox) else {
            Issue.record("expected a task list")
            return
        }
        #expect(tasks.map(\.id) == [outcome.entity])
        await vault.bridge.shutdown()
    }

    /// **A stream opened onto a closed vault is closed, not primed.**
    ///
    /// The prime is an instruction to re-read, and after `shutdown()` there is
    /// nothing left to read it with: `Core.query` returns `CoreError.Closed`,
    /// `CoreBridge.query` does not guard on that, and a model such as
    /// `TaskListModel` paints the throw. So a feed opened during teardown —
    /// a tab appearing as the window goes away, a `follow()` task starting
    /// late — put "core is closed" on screen. Every one of the fourteen
    /// `follow()` loops guards on `isClosed` and would have returned at once;
    /// it was the prime sitting in front of the close batch that made them all
    /// re-read first.
    ///
    /// Bounded through the same helper as the test above, because the way this
    /// fails is that nothing arrives — an unbounded wait would hang the suite
    /// instead of failing it.
    @Test
    func aStreamOpenedAfterShutdownIsClosedRatherThanPrimed() async throws {
        let vault = try await TestVault()
        await vault.bridge.shutdown()

        let changes = await vault.bridge.changes(window: .milliseconds(20))
        let batch = try #require(
            await Self.firstBatch(of: changes),
            "a stream onto a closed vault must still say something, and say it is closed"
        )
        #expect(
            batch.isClosed,
            "the first batch was a prime, so every screen re-read a core that refuses reads"
        )
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
        // A view that appears, repaints once on a real write and disappears.
        // Past the prime for the reason above: an empty opening batch is not
        // the repaint this is about. Bounded like the test above.
        let departed = Task {
            await Self.firstBatch(of: leaving, matching: { !$0.touched.isEmpty })
        }

        let first = try await vault.bridge.submit(.createTask(draft: draft("Pack the tent")))
        #expect(await departed.value != nil, "the leaving screen never saw its write")

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

    /// The first batch a stream produces that `matching` accepts, or `nil` if
    /// it produces none inside `within`. A feed defect shows up as silence,
    /// and silence is a hang unless something bounds it.
    private static func firstBatch(
        of stream: AsyncStream<ChangeBatch>,
        within: Duration = .seconds(3),
        matching: @escaping @Sendable (ChangeBatch) -> Bool = { _ in true }
    ) async -> ChangeBatch? {
        let reader = Task { () -> ChangeBatch? in
            for await batch in stream where matching(batch) { return batch }
            return nil
        }
        let timeout = Task {
            try? await Task.sleep(for: within)
            reader.cancel()
        }
        defer { timeout.cancel() }
        return await reader.value
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
