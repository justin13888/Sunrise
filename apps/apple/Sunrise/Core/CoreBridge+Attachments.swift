import Foundation

// The bridge's attachment pass-throughs, in a file of their own because
// `CoreBridge.swift` was past the length this project lints for. Still the
// actor's own methods: an extension of an actor is isolated to it, so these
// keep the single-owner guarantee the type documents.
extension CoreBridge {
    // MARK: - Attachments

    /// Seal a file's bytes into the vault and record them against a task.
    ///
    /// The bytes go over whole rather than as a path: a file the user picked
    /// arrives with a security scope this process holds and the Rust side
    /// cannot, so reading it is the app's job. `mimeType` is the app's too —
    /// `UTType` is what knows a `.heic` is `image/heic`.
    ///
    /// `preview` carries what the platform could tell about the file: its
    /// pixel size and a JPEG or PNG thumbnail it rendered. The core seals the
    /// thumbnail as a blob of its own (ADR-0053 §1–§2).
    func attachFile(
        to task: EntityRef,
        filename: String,
        mimeType: String,
        bytes: Data,
        preview: AttachPreviewIn
    ) async throws -> AttachmentItem {
        try await core.attachFile(
            task: task,
            filename: filename,
            mimeType: mimeType,
            bytes: bytes,
            preview: preview
        )
    }

    /// One attachment's thumbnail, verified, or `nil` when it has none.
    ///
    /// Throws `BindingError.AttachmentNotHere` until the sync driver has
    /// fetched it.
    func thumbnailBytes(_ id: EntityRef) async throws -> ThumbnailOut? {
        try await core.thumbnailBytes(id: id)
    }

    /// One attachment's plaintext, reassembled and hash-checked by the core.
    ///
    /// Throws `BindingError.AttachmentNotHere` when this device holds the row
    /// and not the chunks — a state to render, not a failure to report.
    func attachmentBytes(_ id: EntityRef) async throws -> Data {
        try await core.attachmentBytes(id: id)
    }

    /// Whether this device holds every chunk of `attachment`.
    func attachmentIsLocal(_ attachment: AttachmentItem) throws -> Bool {
        try core.attachmentIsLocal(attachment: attachment)
    }

    /// Download one attachment's bytes on demand, whatever its size.
    ///
    /// What the Download button calls. Under the core's 10 MiB auto-fetch
    /// threshold nothing needs this — the sync driver fetches those unasked —
    /// and over it this is the only route to the bytes at all.
    ///
    /// Returns when they are here. It has no timeout by design, and cancelling
    /// the Swift `Task` awaiting it will not stop it: a UniFFI async call
    /// carries no cancellation across the seam, so the way to end one is
    /// `cancelAttachmentFetch`.
    ///
    /// Throws `BindingError.AttachmentDownloadCancelled` when that happens,
    /// which is the user's own decision rather than a failure to report.
    func fetchAttachment(_ id: EntityRef) async throws {
        try await core.fetchAttachment(id: id)
    }

    /// Stop a running download and mark the attachment partial.
    ///
    /// Synchronous and immediate: it releases the pending `fetchAttachment`
    /// whether or not a relay is answering, which is the state a user is most
    /// likely to be cancelling from. A no-op for an id with nothing
    /// outstanding.
    func cancelAttachmentFetch(_ id: EntityRef) throws {
        try core.cancelAttachmentFetch(id: id)
    }

    /// This device's cache state for one attachment's bytes.
    ///
    /// Durable, so a row still reads `.partial` after a relaunch — which is
    /// what makes it a cache state rather than a view state, and why the model
    /// reads it back rather than remembering it.
    func attachmentFetchState(_ id: EntityRef) throws -> AttachmentFetchState {
        try core.attachmentFetchState(id: id)
    }

    // MARK: - The attachment cache and the network gate (ADR-0053 §5–§6)

    /// Tell the core what kind of network this device is on; the fetch drain
    /// decides from it what to fetch unasked.
    func setNetworkClass(_ networkClass: NetworkClass) {
        core.setNetworkClass(network: networkClass)
    }

    /// Hold an attachment's bytes in the cache while a preview of them is
    /// open.
    func pinAttachment(_ id: EntityRef) async throws {
        try await core.pinAttachment(id: id)
    }

    /// Release one pin ``pinAttachment(_:)`` took. Reads no row, so it also
    /// releases a pin whose attachment was deleted while its preview was open.
    func unpinAttachment(_ id: EntityRef) {
        core.unpinAttachment(id: id)
    }

    /// What the attachment cache holds, what Clear cache would free, and the
    /// limit.
    func attachmentCacheUsage() throws -> CacheUsageItem {
        try core.attachmentCacheUsage()
    }

    /// Evict everything evictable. Returns the bytes freed.
    func clearAttachmentCache() throws -> UInt64 {
        try core.clearAttachmentCache()
    }

    /// Bring the cache under its limit now, after the limit was lowered.
    func enforceAttachmentCache() throws -> UInt64 {
        try core.enforceAttachmentCache()
    }
}
