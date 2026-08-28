import Foundation
import UniformTypeIdentifiers

/// One attachment row, with the one fact the metadata does not carry.
struct AttachmentRow: Identifiable, Equatable {
    let item: AttachmentItem
    /// Whether this device holds the chunks, not just the metadata.
    ///
    /// Metadata syncs and blob chunks do not, so an attachment made on another
    /// device arrives describable and unopenable. That is a state to render,
    /// not an error to report, which is why it is a field rather than a
    /// failure at open time.
    let isLocal: Bool

    var id: EntityRef { item.id }

    /// What the row shows for its size.
    var sizeText: String {
        ByteCountFormatStyle(style: .file).format(Int64(item.sizeBytes))
    }

    /// Whether this app can draw the bytes itself, rather than handing them to
    /// another one.
    var previewKind: AttachmentPreview {
        let type = UTType(mimeType: item.mimeType)
        if type?.conforms(to: .pdf) == true { return .pdf }
        if type?.conforms(to: .image) == true { return .image }
        return .none
    }
}

/// What the detail pane can render inline.
enum AttachmentPreview {
    case image
    case pdf
    case none
}

/// One task's attachments: list, add, open, remove.
///
/// Every byte-handling step is the core's — sealing, chunking, the content
/// hash, reassembly. This model reads files the user picked (which the app can
/// do and the Rust side cannot, because the security scope belongs to this
/// process) and names their MIME type from `UTType`, which is the platform's
/// job and not a table worth copying into Rust.
@MainActor
@Observable
final class AttachmentsModel {
    private(set) var rows: [AttachmentRow] = []
    private(set) var errorMessage: String?
    /// Bytes of the row being previewed, keyed by id so a stale load cannot
    /// paint over a newer selection.
    private(set) var previewing: (id: EntityRef, data: Data)?
    private(set) var isBusy = false

    private let bridge: CoreBridge
    private let task: EntityRef

    init(bridge: CoreBridge, task: EntityRef) {
        self.bridge = bridge
        self.task = task
    }

    func refresh() async {
        do {
            guard case let .attachments(items) = try await bridge.query(
                .taskAttachments(task: task)
            ) else {
                rows = []
                return
            }
            // One locality check per row. Each is a handful of file-exists
            // calls inside the actor, not a read of the bytes — the whole
            // point of `attachmentIsLocal` existing rather than the list
            // finding out by reassembling every attachment it draws.
            var built: [AttachmentRow] = []
            built.reserveCapacity(items.count)
            for item in items {
                let isLocal = (try? await bridge.attachmentIsLocal(item)) ?? false
                built.append(AttachmentRow(item: item, isLocal: isLocal))
            }
            rows = built
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    /// Attach a file the user picked or dropped.
    ///
    /// Reads the whole file into memory. That is a deliberate ceiling, not an
    /// oversight: `MAX_ATTACHMENT_BYTES` is 100 MB and the core wants the
    /// plaintext to hash it anyway, so streaming would buy nothing and cost a
    /// second code path.
    func attach(contentsOf url: URL) async {
        isBusy = true
        defer { isBusy = false }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        do {
            let bytes = try Data(contentsOf: url)
            _ = try await bridge.attachFile(
                to: task,
                filename: url.lastPathComponent,
                mimeType: Self.mimeType(of: url),
                bytes: bytes
            )
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Load one attachment's bytes for the inline preview.
    func preview(_ row: AttachmentRow) async {
        guard row.isLocal else {
            previewing = nil
            errorMessage = "\(row.item.filename) has not been downloaded to this device."
            return
        }
        do {
            previewing = (row.id, try await bridge.attachmentBytes(row.id))
            errorMessage = nil
        } catch {
            previewing = nil
            errorMessage = error.localizedDescription
        }
    }

    /// Write an attachment to a temporary file and hand it to the system.
    ///
    /// The temporary copy is how anything but this app can open it: the bytes
    /// live sealed in the vault and there is no path to hand over.
    func exportToTemporary(_ row: AttachmentRow) async -> URL? {
        do {
            let bytes = try await bridge.attachmentBytes(row.id)
            let url = FileManager.default.temporaryDirectory
                .appending(path: "sunrise-\(row.id)")
                .appending(path: row.item.filename)
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try bytes.write(to: url)
            return url
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    /// Tombstone one attachment's metadata.
    ///
    /// The blob is not reclaimed here. `docs/02-domain/attachments.md`
    /// §Deletion makes that the relay's job, behind a device-cursor quorum and
    /// a grace period, and a client deleting local chunks would be removing
    /// the only copy that had reached anywhere.
    func detach(_ row: AttachmentRow) async {
        do {
            _ = try await bridge.submit(.detachFile(id: row.id))
            if previewing?.id == row.id { previewing = nil }
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func dismissError() { errorMessage = nil }

    /// The platform's answer for a file's type, `application/octet-stream`
    /// when it has none.
    static func mimeType(of url: URL) -> String {
        (try? url.resourceValues(forKeys: [.contentTypeKey]).contentType)?
            .preferredMIMEType
            ?? UTType(filenameExtension: url.pathExtension)?.preferredMIMEType
            ?? "application/octet-stream"
    }
}
