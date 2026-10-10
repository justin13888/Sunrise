import Foundation
import Testing

@testable import Sunrise

private typealias Surface = SystemSurfaceFixture

/// What the share extension leaves behind, and how the app files it into the
/// Inbox (`mobile-ios.md` §Sharing extension): text, a link and an image, and
/// a filing interrupted half way.
struct SharedCaptureTests {
    private func capture(
        text: String? = nil,
        url: String? = nil,
        images: [PendingCapture.Image] = []
    ) -> PendingCapture {
        PendingCapture(
            id: UUID(),
            createdAtMs: 1_760_000_000_000,
            text: text,
            url: url.flatMap(URL.init(string:)),
            images: images
        )
    }

    private func paragraph(_ text: String) -> NoteBlock {
        .paragraph(inline: [.text(text: text, marks: [])])
    }

    /// Write a capture the way the extension does: images into a `.partial`
    /// directory, then the record, then the rename.
    private func share(
        _ capture: PendingCapture,
        images: [String: Data] = [:],
        into store: PendingCaptureStore
    ) throws {
        let partial = try store.begin(capture.id)
        for (file, data) in images {
            try data.write(to: partial.appending(path: file))
        }
        try store.commit(capture, from: partial)
    }

    // MARK: - What a capture becomes

    @Test
    func sharedTextBecomesATitleAndANote() {
        let text = "Book the ferry\n\nCheck the times first.\nAnd the price."
        let draft = SharedCapture.draft(for: capture(text: text))
        #expect(draft.title == "Book the ferry")
        #expect(draft.body == [paragraph("Check the times first."), paragraph("And the price.")])
    }

    /// A browser shares the page title as text beside the link: the title is
    /// the task, and the link goes in the note.
    @Test
    func aSharedPageIsItsTitleWithTheLinkInTheNote() {
        let link = "https://www.example.com/articles/1"
        let draft = SharedCapture.draft(for: capture(text: "A long read", url: link))
        #expect(draft.title == "A long read")
        #expect(draft.body == [.paragraph(inline: [.link(href: link, label: link)])])
    }

    @Test
    func aBareLinkIsTitledByItsHostAndPath() {
        #expect(SharedCapture.draft(for: capture(url: "https://www.example.com/articles/1")).title
            == "example.com/articles/1")
        #expect(SharedCapture.draft(for: capture(url: "https://example.com/")).title == "example.com")
    }

    @Test
    func anImageAloneIsTitledAsOne() {
        let one = PendingCapture.Image(file: "image-0.jpg", name: "IMG_1.jpg")
        let two = PendingCapture.Image(file: "image-1.png", name: "IMG_2.png")
        #expect(SharedCapture.draft(for: capture(images: [one])).title == "Shared image")
        #expect(SharedCapture.draft(for: capture(images: [one, two])).title == "Shared images (2)")
        #expect(SharedCapture.draft(for: capture(images: [one])).body.isEmpty)
    }

    /// No capture parser: a shared paragraph is somebody else's prose, and a
    /// `#` in it is not a stream.
    @Test
    func sharedTextIsNotParsedForTags() {
        #expect(SharedCapture.draft(for: capture(text: "Read #general ^tomorrow !1")).title
            == "Read #general ^tomorrow !1")
    }

    /// A first line too long for a title is clipped there and kept whole in
    /// the note, so nothing shared is lost.
    @Test
    func aLongFirstLineIsClippedAndKeptInTheNote() {
        let line = String(repeating: "word ", count: 80).trimmingCharacters(in: .whitespaces)
        let draft = SharedCapture.draft(for: capture(text: line))
        #expect(draft.title.count == SharedCapture.titleLimit)
        #expect(draft.title.hasSuffix("…"))
        #expect(draft.body == [paragraph(line)])
    }

    @Test
    func mimeTypesComeFromTheFileName() {
        #expect(SharedCapture.mimeType(of: "IMG_1.jpg") == "image/jpeg")
        #expect(SharedCapture.mimeType(of: "Screenshot.png") == "image/png")
        #expect(SharedCapture.mimeType(of: "mystery") == "application/octet-stream")
    }

    // MARK: - The store

    /// The app never sees half a capture: a `.partial` directory is not
    /// pending, and an abandoned one is swept after a day.
    @Test
    func onlyCommittedCapturesArePending() throws {
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let first = capture(text: "First")
        try share(first, into: store)
        _ = try store.begin(UUID())
        #expect(store.pending().map(\.id) == [first.id])

        store.sweepAbandoned(now: Date().addingTimeInterval(PendingCaptureStore.abandonedAfter + 60))
        let left = try FileManager.default.contentsOfDirectory(atPath: directory.path)
        #expect(left == [first.id.uuidString], "the abandoned partial is gone and the capture is not")
    }

    // MARK: - Filing

    /// Text, a link and an image, filed into a real vault: one Inbox task with
    /// the note and the attachment, and the capture gone from the store.
    @Test
    func aShareIsFiledIntoTheInboxWithItsImage() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let image = PendingCapture.Image(file: "image-0.png", name: "Receipt.png")
        let bytes = Data("not really a png".utf8)
        let shared = capture(text: "Expense this", url: "https://example.com/receipt", images: [image])
        try share(shared, images: ["image-0.png": bytes], into: store)
        try share(capture(text: "Second share"), into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.failures.isEmpty)
        #expect(report.filed.count == 2)
        #expect(store.pending().isEmpty)

        let inbox = try await Surface.inbox(vault.bridge)
        #expect(Set(inbox.map(\.title)) == ["Expense this", "Second share"])
        let filed = try #require(inbox.first { $0.title == "Expense this" })
        let note = decodeNoteBody(body: try #require(filed.body))
        #expect(note.blocks == [.paragraph(inline: [
            .link(href: "https://example.com/receipt", label: "https://example.com/receipt")
        ])])
        let attached = try await vault.bridge.query(.taskAttachments(task: filed.id))
        guard case let .attachments(items) = attached else {
            Issue.record("expected the task's attachments, got \(attached)")
            return
        }
        #expect(items.map(\.filename) == ["Receipt.png"])
        #expect(items.map(\.mimeType) == ["image/png"])
        #expect(try await vault.bridge.attachmentBytes(items[0].id) == bytes)
        await vault.bridge.shutdown()
    }

    /// A filing interrupted after the task was made resumes on that task: the
    /// image still to go is attached to it, and no second task appears.
    @Test
    func anInterruptedFilingResumesOnTheSameTask() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let task = try await Surface.task("Shared image", in: vault.bridge)
        var halfway = capture(images: [PendingCapture.Image(file: "image-0.jpg", name: "Photo.jpg")])
        halfway.filedAs = task
        try share(halfway, images: ["image-0.jpg": Data("jpeg".utf8)], into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.filed == [task])
        #expect(try await Surface.inbox(vault.bridge).count == 1, "resumed, not filed twice")
        guard case let .attachments(items) = try await vault.bridge.query(.taskAttachments(task: task)) else {
            Issue.record("expected the task's attachments")
            return
        }
        #expect(items.map(\.filename) == ["Photo.jpg"])
        #expect(store.pending().isEmpty)
        await vault.bridge.shutdown()
    }

    /// An image the record lists and the folder does not hold is dropped, and
    /// the rest of the capture is filed: retrying it would fail on every open
    /// forever and keep the capture out of the Inbox.
    @Test
    func aMissingImageIsDroppedAndTheRestFiled() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let missing = capture(text: "Photo of the whiteboard", images: [
            PendingCapture.Image(file: "image-0.jpg", name: "Board.jpg")
        ])
        try share(missing, into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.failures.isEmpty)
        #expect(report.filed.count == 1)
        #expect(store.pending().isEmpty)
        #expect(try await Surface.inbox(vault.bridge).map(\.title) == ["Photo of the whiteboard"])
        await vault.bridge.shutdown()
    }

    // MARK: - Refusals that would recur

    /// The core's ceiling is 100 MB in decimal (`MAX_ATTACHMENT_BYTES`), and
    /// it refuses an empty file. An image between 100 MB and 100 MiB passed
    /// the share sheet and was refused on every open.
    @Test
    func theRefusalsAreTheCoresOwn() {
        #expect(PendingCapture.maxImageBytes == 100_000_000)
        #expect(SharedCapture.refusal(size: 0) == .empty)
        #expect(SharedCapture.refusal(size: 1) == nil)
        #expect(SharedCapture.refusal(size: 100_000_000) == nil)
        #expect(SharedCapture.refusal(size: 100_000_001) == .tooLarge)
        #expect(SharedCapture.refusal(size: 100 * 1024 * 1024) == .tooLarge)
    }

    /// An image the core would refuse on every open leaves the capture before
    /// the task is made, and the note says it is not coming. The capture is
    /// filed, not failed, so nothing is retried forever.
    @Test
    func anImageTheCoreWouldAlwaysRefuseIsNotedAndNotRetried() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let empty = PendingCapture.Image(file: "image-0.jpg", name: "Scan.jpg")
        let kept = PendingCapture.Image(file: "image-1.jpg", name: "Page.jpg")
        let shared = capture(text: "Scanned letter", images: [empty, kept])
        try share(shared, images: ["image-0.jpg": Data(), "image-1.jpg": Data("jpeg".utf8)], into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.failures.isEmpty, "a refusal that would recur is not a failure to retry")
        #expect(report.filed.count == 1)
        #expect(store.pending().isEmpty)

        let filed = try #require(try await Surface.inbox(vault.bridge).first)
        let note = decodeNoteBody(body: try #require(filed.body))
        #expect(note.blocks == [paragraph(SharedCapture.notice("Scan.jpg", .empty))])
        let attached = try await vault.bridge.query(.taskAttachments(task: filed.id))
        guard case let .attachments(items) = attached else {
            Issue.record("expected the task's attachments")
            return
        }
        #expect(items.map(\.filename) == ["Page.jpg"])
        await vault.bridge.shutdown()
    }

    /// A resumed filing drops the refused image from the record too, so the
    /// next pass does not meet it again.
    @Test
    func aResumedFilingDropsARefusedImage() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)

        let task = try await Surface.task("Shared image", in: vault.bridge)
        var halfway = capture(images: [PendingCapture.Image(file: "image-0.jpg", name: "Blank.jpg")])
        halfway.filedAs = task
        try share(halfway, images: ["image-0.jpg": Data()], into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.failures.isEmpty)
        #expect(report.filed == [task])
        #expect(store.pending().isEmpty)
        await vault.bridge.shutdown()
    }

    /// A name past the core's 256 is shortened with its extension kept, not
    /// refused: the core would refuse it on every open.
    @Test
    func aLongImageNameIsShortenedNotRefused() async throws {
        let long = String(repeating: "a", count: 300) + ".png"
        let fitted = SharedCapture.attachmentName(long)
        #expect(fitted.unicodeScalars.count == PendingCapture.maxImageNameLength)
        #expect(fitted.hasSuffix(".png"))
        #expect(SharedCapture.attachmentName("  Receipt.png ") == "Receipt.png")
        #expect(SharedCapture.attachmentName("   ") == "Shared image")
        let emoji = String(repeating: "🌅", count: 300)
        #expect(SharedCapture.attachmentName(emoji).unicodeScalars.count == PendingCapture.maxImageNameLength)

        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)
        let shared = capture(images: [PendingCapture.Image(file: "image-0.png", name: long)])
        try share(shared, images: ["image-0.png": Data("png".utf8)], into: store)

        let report = await SharedCapture.fileAll(from: store, into: vault.bridge)
        #expect(report.failures.isEmpty)
        let task = try #require(report.filed.first)
        guard case let .attachments(items) = try await vault.bridge.query(.taskAttachments(task: task)) else {
            Issue.record("expected the task's attachments")
            return
        }
        #expect(items.map(\.filename) == [fitted])
        #expect(items.map(\.mimeType) == ["image/png"])
        await vault.bridge.shutdown()
    }
}

#if os(iOS)
/// When the app files what was shared (`iOS/ShareInbox.swift`): a vault
/// opening and the scene becoming active usually fire together, and the two
/// passes must not file one share twice.
@MainActor
struct ShareInboxTests {
    @Test
    func twoTriggersTogetherFileAShareOnce() async throws {
        let vault = try await TestVault()
        let directory = Surface.scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PendingCaptureStore(directory: directory)
        let shared = PendingCapture(id: UUID(), createdAtMs: 1_760_000_000_000, text: "Shared once")
        try store.commit(shared, from: try store.begin(shared.id))

        let inbox = ShareInbox(store: store)
        async let opened: Void = inbox.file(into: vault.bridge)
        async let active: Void = inbox.file(into: vault.bridge)
        _ = await (opened, active)

        #expect(try await Surface.inbox(vault.bridge).map(\.title) == ["Shared once"])
        #expect(store.pending().isEmpty)

        // A later trigger with nothing waiting files nothing more.
        await inbox.file(into: vault.bridge)
        #expect(try await Surface.inbox(vault.bridge).count == 1)
        await vault.bridge.shutdown()
    }
}
#endif
