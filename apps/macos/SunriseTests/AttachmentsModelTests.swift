import Foundation
import Testing

@testable import Sunrise

/// Attachments against a real vault, which is the only way to test them: the
/// sealing, the chunking and the content hash all live in the core, and the
/// claim being made is that the app can reach them.
@MainActor
struct AttachmentsModelTests {
    private func temporaryFile(named name: String, bytes: Data) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-attach-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        let file = url.appending(path: name)
        try bytes.write(to: file)
        return file
    }

    private func aTask(_ vault: borrowing TestVault) async throws -> EntityRef {
        try await vault.bridge.submit(
            .createTask(
                draft: TaskDraftIn(
                    title: "File the tax return",
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
            )
        ).entity
    }

    @Test
    func afileAttachesAndComesBackByteForByte() async throws {
        let vault = try await TestVault()
        let task = try await aTask(vault)
        let model = AttachmentsModel(bridge: vault.bridge, task: task)
        await model.refresh()
        #expect(model.rows.isEmpty)

        let bytes = Data("%PDF-1.7 a small return".utf8)
        let file = try temporaryFile(named: "return.pdf", bytes: bytes)
        await model.attach(contentsOf: file)

        let row = try #require(model.rows.first)
        #expect(row.item.filename == "return.pdf")
        #expect(row.item.mimeType == "application/pdf")
        #expect(row.item.sizeBytes == UInt64(bytes.count))
        #expect(row.isLocal)
        #expect(row.previewKind == .pdf)

        await model.preview(row)
        #expect(model.previewing?.data == bytes)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// The multi-chunk path, which is where an off-by-one in the reassembly
    /// would hide. 256 KiB is the chunk size.
    @Test
    func aFileLargerThanOneChunkRoundTrips() async throws {
        let vault = try await TestVault()
        let task = try await aTask(vault)
        let model = AttachmentsModel(bridge: vault.bridge, task: task)

        var bytes = Data(count: 0)
        bytes.reserveCapacity(600 * 1024)
        for index in 0..<(600 * 1024) { bytes.append(UInt8(index % 251)) }
        let file = try temporaryFile(named: "scan.png", bytes: bytes)
        await model.attach(contentsOf: file)

        let row = try #require(model.rows.first)
        #expect(row.item.chunkCount == 3)
        #expect(row.previewKind == .image)
        await model.preview(row)
        #expect(model.previewing?.data == bytes)
        await vault.bridge.shutdown()
    }

    /// A file whose type this app cannot draw still attaches and still opens —
    /// in whatever owns it. Guessing at a renderer is how a `.zip` gets shown
    /// as mojibake.
    @Test
    func anUnpreviewableTypeStillAttachesAndExports() async throws {
        let vault = try await TestVault()
        let task = try await aTask(vault)
        let model = AttachmentsModel(bridge: vault.bridge, task: task)

        let bytes = Data("id,name\n1,ada\n".utf8)
        let file = try temporaryFile(named: "people.csv", bytes: bytes)
        await model.attach(contentsOf: file)

        let row = try #require(model.rows.first)
        #expect(row.previewKind == .none)
        let exported = try #require(await model.exportToTemporary(row))
        #expect(try Data(contentsOf: exported) == bytes)
        #expect(exported.lastPathComponent == "people.csv")
        await vault.bridge.shutdown()
    }

    @Test
    func removingAnAttachmentTakesItOffTheList() async throws {
        let vault = try await TestVault()
        let task = try await aTask(vault)
        let model = AttachmentsModel(bridge: vault.bridge, task: task)
        let file = try temporaryFile(named: "notes.txt", bytes: Data("hello".utf8))
        await model.attach(contentsOf: file)
        let row = try #require(model.rows.first)
        await model.preview(row)

        await model.detach(row)

        #expect(model.rows.isEmpty)
        #expect(model.previewing == nil, "the preview does not outlive its row")
        await vault.bridge.shutdown()
    }

    /// An empty file is refused by the core rather than recorded as an
    /// attachment with nothing behind it, and the app reports it rather than
    /// showing an empty row.
    @Test
    func anEmptyFileIsRefusedAndReported() async throws {
        let vault = try await TestVault()
        let task = try await aTask(vault)
        let model = AttachmentsModel(bridge: vault.bridge, task: task)

        let file = try temporaryFile(named: "empty.txt", bytes: Data())
        await model.attach(contentsOf: file)

        #expect(model.rows.isEmpty)
        #expect(model.errorMessage != nil)
        await vault.bridge.shutdown()
    }

    /// The types come from `UTType`, which is the platform's table and not one
    /// worth copying into Rust.
    @Test
    func theMimeTypeComesFromTheSystemsOwnTable() {
        #expect(AttachmentsModel.mimeType(of: URL(filePath: "/tmp/a.pdf")) == "application/pdf")
        #expect(AttachmentsModel.mimeType(of: URL(filePath: "/tmp/a.png")) == "image/png")
        #expect(
            AttachmentsModel.mimeType(of: URL(filePath: "/tmp/a.unknownzz"))
                == "application/octet-stream"
        )
    }
}

/// The activity timeline, which had no reader at all until now: every
/// completion, deferral and move was recorded in the op log and unreachable
/// from the app.
@MainActor
struct ActivityModelTests {
    @Test
    func aTasksHistoryIsReadBackInTheDomainsOwnWords() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(
            TaskDraftIn(
                title: "Renew the passport",
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
        )
        let task = try #require(list.tasks.first)
        await list.defer_(task, byDays: 1)
        await list.complete(try #require(list.tasks.first))

        let model = ActivityModel(bridge: vault.bridge, entity: task.id)
        await model.refresh()

        #expect(model.rows.count >= 3)
        // Newest first.
        #expect(model.rows.first?.detail == .taskCompleted)
        #expect(model.rows.last?.detail == .taskCreated)
        // Every row is worded by the seam, not here.
        #expect(model.rows.allSatisfy { !$0.phrase.isEmpty })
        #expect(model.rows.allSatisfy { $0.entity == task.id })

        let device = await model.deviceIdentifier()
        #expect(model.rows.allSatisfy { model.isThisDevice($0, deviceID: device) })
        #expect(model.day(of: try #require(model.rows.first)).text == "today")
        await vault.bridge.shutdown()
    }

    /// An empty id is not "this device": a feed that had not yet learned its
    /// own id would otherwise claim every row.
    @Test
    func anUnknownDeviceIdMatchesNothing() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(
            TaskDraftIn(
                title: "Book the ferry",
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
        )
        let task = try #require(list.tasks.first)
        let model = ActivityModel(bridge: vault.bridge, entity: task.id)
        await model.refresh()

        #expect(model.rows.allSatisfy { !model.isThisDevice($0, deviceID: "") })
        await vault.bridge.shutdown()
    }
}
