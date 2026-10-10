import Foundation
import UniformTypeIdentifiers

/// Filing what the share extension left in the App Group into the Inbox.
///
/// `docs/07-clients/mobile-ios.md` §Sharing extension: shared text, a link or
/// an image becomes a task in the Inbox, with the text and the link as its note
/// and the image as an attachment. The extension cannot write the vault (see
/// ``PendingCapture``), so this runs in the app, through the same commands
/// every other capture uses.
///
/// **No capture parser.** A shared paragraph is somebody else's prose, and
/// reading a `#` or a `^` in it as a tag would file a quoted article under a
/// stream it happened to mention. What is shared is filed as written.
enum SharedCapture {
    /// The longest title a shared line becomes. The core allows more; a
    /// title is a line in a list, and anything past this is in the note in
    /// full.
    static let titleLimit = 200

    /// What a capture is filed as, before anything touches the vault.
    struct Draft: Equatable {
        let title: String
        let body: [NoteBlock]
    }

    /// The task one capture becomes.
    ///
    /// The title is the first line of the text; failing that, the link's host
    /// and path; failing that, what was shared. Every other line of the text
    /// is a paragraph of the note, and the link follows them — so nothing
    /// shared is lost to the title's length. Last come the `refused` notices,
    /// one paragraph each, for the images the core would never take.
    static func draft(for capture: PendingCapture, refused: [String] = []) -> Draft {
        let lines = (capture.text ?? "")
            .components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
        var body: [NoteBlock] = []
        let title: String
        if let first = lines.first {
            title = clipped(first)
            // A clipped first line goes into the note whole.
            let rest = title == first ? lines.dropFirst() : lines[...]
            body += rest.map { .paragraph(inline: [.text(text: $0, marks: [])]) }
        } else if let url = capture.url {
            title = clipped(linkTitle(url))
        } else {
            title = capture.images.count > 1 ? "Shared images (\(capture.images.count))" : "Shared image"
        }
        if let url = capture.url {
            body.append(.paragraph(inline: [.link(href: url.absoluteString, label: url.absoluteString)]))
        }
        body += refused.map { .paragraph(inline: [.text(text: $0, marks: [])]) }
        return Draft(title: title, body: body)
    }

    /// Why the core would refuse an image on every attempt.
    enum Refusal: Equatable {
        /// `AttachError::Empty`: no bytes.
        case empty
        /// `AttachError::TooLarge`: over ``PendingCapture/maxImageBytes``.
        case tooLarge
    }

    /// The refusal an image of `size` bytes meets, or `nil` if the core
    /// accepts it. The core's own two checks, asked before it is, so that an
    /// image it can never take is told apart from an attach that failed for
    /// now.
    static func refusal(size: Int) -> Refusal? {
        if size <= 0 { return .empty }
        if size > PendingCapture.maxImageBytes { return .tooLarge }
        return nil
    }

    /// The sentence a refused image leaves in the task's note, which is where
    /// the user looks for the image and so where they learn it is not coming.
    static func notice(_ name: String, _ refusal: Refusal) -> String {
        switch refusal {
        case .empty: "Not attached: “\(name)” was empty."
        case .tooLarge: "Not attached: “\(name)” is larger than the 100 MB an attachment can be."
        }
    }

    /// The name an image is attached under: what the sharing app called it,
    /// trimmed, and shortened to the core's ``PendingCapture/maxImageNameLength``
    /// with its extension kept. The core refuses a longer name, and it would
    /// refuse it on every open, so it is fitted rather than refused.
    static func attachmentName(_ name: String) -> String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "Shared image" }
        let limit = PendingCapture.maxImageNameLength
        guard trimmed.unicodeScalars.count > limit else { return trimmed }
        let ext = (trimmed as NSString).pathExtension
        // An "extension" this long is not one, and keeping it whole would
        // leave no room for the name.
        let suffix = ext.isEmpty || ext.unicodeScalars.count > 16 ? "" : "." + ext
        let base = suffix.isEmpty ? trimmed : (trimmed as NSString).deletingPathExtension
        var kept = String.UnicodeScalarView()
        kept.append(contentsOf: base.unicodeScalars.prefix(limit - suffix.unicodeScalars.count))
        return String(kept).trimmingCharacters(in: .whitespacesAndNewlines) + suffix
    }

    /// A link, as a title: `example.com/some/article`.
    static func linkTitle(_ url: URL) -> String {
        guard let host = url.host(), !host.isEmpty else { return url.absoluteString }
        let bare = host.hasPrefix("www.") ? String(host.dropFirst(4)) : host
        let path = url.path()
        return path.isEmpty || path == "/" ? bare : bare + path
    }

    static func clipped(_ line: String) -> String {
        line.count > titleLimit ? String(line.prefix(titleLimit - 1)) + "…" : line
    }

    /// The MIME type an attachment is recorded with, from the file's name.
    static func mimeType(of name: String) -> String {
        UTType(filenameExtension: (name as NSString).pathExtension)?.preferredMIMEType
            ?? "application/octet-stream"
    }

    /// File one capture, resuming wherever an earlier attempt stopped.
    ///
    /// The task is created first and its id written back into the record
    /// before any image is attached, and each image leaves the record as it
    /// lands. A filing interrupted at any point — the app suspended, an
    /// attachment refused — therefore resumes on the same task with only the
    /// images still to go, rather than filing the share twice. The record is
    /// removed only once nothing is left.
    ///
    /// **A refusal that would recur is not retried.** An image the core can
    /// never take (``refusal(size:)``) leaves the record before anything is
    /// written, and the note says it was not attached. Retrying it would fail
    /// on every open forever, with the task already in the Inbox and nothing
    /// to tell the user why its image never arrived. Any other failure throws,
    /// and the capture is tried again on the next pass.
    @discardableResult
    static func file(
        _ capture: PendingCapture,
        from store: PendingCaptureStore,
        into bridge: CoreBridge
    ) async throws -> EntityRef {
        let shared = capture
        var capture = capture
        var refused: [String] = []
        capture.images = shared.images.filter { image in
            guard let size = store.imageSize(image, of: shared),
                  let refusal = refusal(size: size) else { return true }
            refused.append(notice(attachmentName(image.name), refusal))
            return false
        }
        let task: EntityRef
        if let filed = capture.filedAs {
            task = filed
            // Filed by an earlier pass, so the note is written; the refused
            // images still leave the record, or the next pass meets them too.
            if !refused.isEmpty { try store.update(capture) }
        } else {
            let draft = draft(for: capture, refused: refused)
            let outcome = try await bridge.submit(.createTask(draft: TaskDraftIn(
                title: draft.title,
                body: draft.body.isEmpty ? nil : encodeNoteBody(blocks: draft.body),
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
            )))
            task = outcome.entity
            capture.filedAs = task
            try store.update(capture)
        }
        while let image = capture.images.first {
            let bytes: Data
            do {
                bytes = try store.imageData(image, of: capture)
            } catch let error as CocoaError where error.code == .fileReadNoSuchFile {
                // Listed and not there: nothing a later pass could attach, so
                // it is dropped rather than retried on every open forever.
                capture.images.removeFirst()
                try store.update(capture)
                continue
            }
            let name = attachmentName(image.name)
            _ = try await bridge.attachFile(
                to: task,
                filename: name,
                mimeType: mimeType(of: name),
                bytes: bytes
            )
            capture.images.removeFirst()
            try store.update(capture)
        }
        try store.remove(capture)
        return task
    }

    /// What one pass over the store did.
    struct Report: Equatable {
        var filed: [EntityRef] = []
        /// Why the captures left behind were not filed. They stay in the
        /// store and are tried again on the next pass, which is right only
        /// because a refusal that would recur never lands here: ``file(_:from:into:)``
        /// drops it and says so in the task's note.
        var failures: [String] = []
    }

    /// File everything pending, oldest first.
    ///
    /// One failure does not stop the rest: a capture that fails for now is
    /// left for the next pass, and the ones after it are filed now.
    static func fileAll(from store: PendingCaptureStore, into bridge: CoreBridge) async -> Report {
        store.sweepAbandoned()
        var report = Report()
        for capture in store.pending() {
            guard !capture.isEmpty || capture.filedAs != nil else {
                try? store.remove(capture)
                continue
            }
            do {
                report.filed.append(try await file(capture, from: store, into: bridge))
            } catch {
                report.failures.append(error.localizedDescription)
            }
        }
        return report
    }
}
