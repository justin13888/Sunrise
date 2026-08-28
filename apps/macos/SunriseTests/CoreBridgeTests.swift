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
