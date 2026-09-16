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
    /// This device's cache state for the bytes, read back from the vault.
    ///
    /// Read rather than remembered, because it outlives the view: a download
    /// interrupted yesterday is still `.partial` when the app reopens, and a
    /// row that forgot would draw a plain Download button over a cache entry
    /// the core knows is half a story.
    let fetchState: AttachmentFetchState

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

/// Where one attachment's bytes stand, from the row's point of view.
enum AttachmentTransfer: Equatable {
    /// Every chunk is on this device. The row opens.
    case here
    /// A download is running. The row shows progress and a Cancel button.
    case running
    /// A download was started and abandoned — the document's `partial: true`.
    /// Pressing Download again restarts it from byte 0.
    case interrupted
    /// Nothing has been asked for. The placeholder and its Download button.
    case absent
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
    /// The `fetchAttachment` calls in flight, one per row.
    ///
    /// Kept outside `rows` because `refresh` rebuilds that array wholesale on
    /// every change batch, and a download is not a fact about the row it is
    /// downloading — it is a fact about this window. Keyed so a second press
    /// on a row already downloading is a no-op rather than a second transfer.
    private var downloads: [EntityRef: _Concurrency.Task<Void, Never>] = [:]

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
                let state = (try? await bridge.attachmentFetchState(item.id)) ?? .idle
                built.append(AttachmentRow(item: item, isLocal: isLocal, fetchState: state))
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

    /// Download one attachment's bytes, whatever its size.
    ///
    /// `docs/02-domain/attachments.md` §Lazy fetch: anything over the 10 MiB
    /// auto-fetch threshold "shows an inline placeholder with file name, size,
    /// and a 'Download' button". This is behind that button. Under the
    /// threshold the sync driver has already fetched it, so the button is not
    /// drawn and this is not reached.
    ///
    /// Fire-and-forget: the call outlives any one frame, and the row redraws
    /// from `downloads` while it runs.
    func download(_ row: AttachmentRow) {
        guard downloads[row.id] == nil else { return }
        let id = row.id
        downloads[id] = _Concurrency.Task { [weak self] in
            await self?.runDownload(id)
        }
    }

    /// What `row` offers to do about its bytes.
    ///
    /// On the model rather than on the row because it is not a fact about the
    /// attachment: a press of Download is known here a frame before the vault
    /// has a row to read it from, and `refresh` rebuilds `rows` only when the
    /// change feed says something happened — which pressing a button does not.
    /// Reading `downloads` here is also what makes the press redraw at all,
    /// since that is the observation the view registers.
    ///
    /// The durable state still has the last word on the two cases this window
    /// cannot know: a request left standing by a previous launch is still
    /// running, and `.partial` is the interrupted transfer the document says a
    /// second press restarts from byte 0.
    func transfer(_ row: AttachmentRow) -> AttachmentTransfer {
        if row.isLocal { return .here }
        if downloads[row.id] != nil { return .running }
        switch row.fetchState {
        case .requested: return .running
        case .partial: return .interrupted
        case .idle: return .absent
        }
    }

    /// Stop a running download.
    ///
    /// Goes through the core rather than cancelling the Swift `Task`: a UniFFI
    /// async call carries no cancellation across the seam, so cancelling the
    /// task here would abandon the caller and leave the transfer running. The
    /// core's cancel is what actually ends it, and it does not need the relay
    /// to agree — which matters, because a stalled transfer is the one people
    /// press this on.
    ///
    /// Spawned rather than awaited, because the caller is a button. The core's
    /// half is synchronous — it takes no network and no lock a download holds —
    /// so this is one hop across the bridge actor and not a wait on anything.
    func cancelDownload(_ row: AttachmentRow) {
        let bridge = self.bridge
        let id = row.id
        _Concurrency.Task { [weak self] in
            do {
                try await bridge.cancelAttachmentFetch(id)
            } catch {
                self?.errorMessage = error.localizedDescription
            }
        }
    }

    private func runDownload(_ id: EntityRef) async {
        var failure: String?
        do {
            try await bridge.fetchAttachment(id)
        } catch let error as BindingError {
            // The user pressed Cancel. Showing that back to them in an error
            // banner is the failure this variant exists to prevent, which is
            // why the seam keeps it apart from every other attachment error.
            if case .AttachmentDownloadCancelled = error {
                failure = nil
            } else {
                failure = error.localizedDescription
            }
        } catch {
            failure = error.localizedDescription
        }
        // Cleared before the refresh, so the row that redraws is the row as it
        // now is rather than one still claiming to be downloading.
        downloads[id] = nil
        await refresh()
        // *After* the refresh, and that order is the whole of it: `refresh`
        // clears `errorMessage` when it succeeds, so reporting first would let
        // the redraw erase the reason for the redraw. Assigned rather than
        // conditionally set, so a download that worked also clears a stale
        // banner from the one before it.
        errorMessage = failure
    }

    /// Load one attachment's bytes for the inline preview.
    func preview(_ row: AttachmentRow) async {
        guard row.isLocal else {
            previewing = nil
            errorMessage = "\(row.item.filename) is not on this device yet. Use Download."
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
            // A download of something being removed is work nobody wants
            // finished, and its `fetchAttachment` would otherwise outlive the
            // row and report against an id the pane no longer draws.
            if downloads[row.id] != nil { cancelDownload(row) }
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
