import Foundation

/// Something shared into Sunrise from another app, waiting for the app to file
/// it in the Inbox.
///
/// `docs/07-clients/mobile-ios.md` §Sharing extension. **The share extension
/// never opens the vault.** It could not do so safely: the app holds the vault
/// lock while it runs, the vault root is in the app's Keychain items, and an
/// extension's memory ceiling is far below what `Core::open` needs. So the
/// extension writes what it was handed into the App Group container as one of
/// these, and the app files it — through the ordinary seam, as a task in the
/// Inbox — the next time it opens a vault.
///
/// Compiled into the extension and both apps, so it imports Foundation and
/// nothing else: no seam types.
struct PendingCapture: Codable, Equatable, Sendable, Identifiable {
    /// Bumped whenever a field changes meaning. A record from another version
    /// is left where it is rather than misread.
    static let currentVersion = 1

    /// One image, copied beside the record.
    struct Image: Codable, Equatable, Sendable {
        /// The copy's name inside the capture's directory.
        let file: String
        /// What the sharing app called it, which becomes the attachment's
        /// filename.
        let name: String
    }

    var version = PendingCapture.currentVersion
    let id: UUID
    /// When it was shared, as Unix milliseconds. Captures are filed oldest
    /// first, so the Inbox reads in the order things were shared.
    let createdAtMs: Int64
    /// Shared text, verbatim. A page shared from a browser carries its title
    /// here beside ``url``.
    var text: String?
    /// A shared web link.
    var url: URL?
    /// Images not yet attached. The app removes each one as it lands, so a
    /// filing interrupted half way attaches only what is left.
    var images: [Image] = []
    /// The task the app filed this as, once it has. Set before any image is
    /// attached, so an interrupted filing resumes on the same task rather than
    /// creating a second one.
    var filedAs: String?

    var isEmpty: Bool {
        (text?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true)
            && url == nil
            && images.isEmpty
    }
}

/// Where pending captures live, and the only code that touches them.
///
/// Each capture is a directory named by its id, holding `capture.json` and its
/// images. The extension builds one under a `.partial` name and renames it into
/// place only once everything is written, so the app never sees half a capture:
/// a rename within one directory is atomic.
struct PendingCaptureStore: Sendable {
    static let folderName = "pending-captures"
    static let recordName = "capture.json"
    static let partialSuffix = ".partial"
    /// The `Info.plist` key carrying the App Group, as ``WidgetSnapshotStore``
    /// reads it. Spelled again here because the share extension compiles none
    /// of the widget sources.
    static let groupInfoKey = "SunriseAppGroup"
    /// How long a `.partial` directory may sit before it is taken for the
    /// remains of an extension that was killed mid-write.
    static let abandonedAfter: TimeInterval = 24 * 60 * 60

    /// The `pending-captures` folder itself.
    let directory: URL

    /// The store in this process's App Group container, or `nil` where the
    /// process was not granted the group. iOS answers `nil` from
    /// `containerURL` itself in that case.
    static func appGroup(bundle: Bundle = .main) -> PendingCaptureStore? {
        guard let group = bundle.object(forInfoDictionaryKey: groupInfoKey) as? String,
              !group.isEmpty,
              let container = FileManager.default.containerURL(
                  forSecurityApplicationGroupIdentifier: group
              ) else { return nil }
        return PendingCaptureStore(directory: container.appending(path: folderName))
    }

    func folder(for id: UUID) -> URL {
        directory.appending(path: id.uuidString)
    }

    // MARK: - Writing (the extension)

    /// Start a capture: an empty `.partial` directory to copy images into.
    func begin(_ id: UUID) throws -> URL {
        let partial = directory.appending(path: id.uuidString + Self.partialSuffix)
        try FileManager.default.createDirectory(at: partial, withIntermediateDirectories: true)
        return partial
    }

    /// Write the record into a `.partial` directory and publish it.
    func commit(_ capture: PendingCapture, from partial: URL) throws {
        try write(capture, into: partial)
        try FileManager.default.moveItem(at: partial, to: folder(for: capture.id))
    }

    /// Throw away a capture that was never committed.
    func discard(partial: URL) {
        try? FileManager.default.removeItem(at: partial)
    }

    // MARK: - Reading (the app)

    /// Every committed capture, oldest first.
    ///
    /// A directory without a readable record of this version is skipped, not
    /// removed: it may be a newer build's, and the user's data is not this
    /// build's to throw away.
    func pending() -> [PendingCapture] {
        let entries = (try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: nil
        )) ?? []
        return entries
            .filter { !$0.lastPathComponent.hasSuffix(Self.partialSuffix) }
            .compactMap { read(from: $0) }
            .sorted { ($0.createdAtMs, $0.id.uuidString) < ($1.createdAtMs, $1.id.uuidString) }
    }

    /// One image's bytes.
    func imageData(_ image: PendingCapture.Image, of capture: PendingCapture) throws -> Data {
        try Data(contentsOf: folder(for: capture.id).appending(path: image.file))
    }

    /// Record progress: the task it was filed as, and the images still to go.
    /// Images no longer listed are deleted.
    func update(_ capture: PendingCapture) throws {
        let folder = folder(for: capture.id)
        try write(capture, into: folder)
        let kept = Set(capture.images.map(\.file) + [Self.recordName])
        let present = (try? FileManager.default.contentsOfDirectory(atPath: folder.path)) ?? []
        for name in present where !kept.contains(name) {
            try? FileManager.default.removeItem(at: folder.appending(path: name))
        }
    }

    /// The capture is filed; forget it.
    func remove(_ capture: PendingCapture) throws {
        do {
            try FileManager.default.removeItem(at: folder(for: capture.id))
        } catch CocoaError.fileNoSuchFile {
            return
        }
    }

    /// Remove `.partial` directories older than ``abandonedAfter``: the
    /// remains of an extension the system ended mid-write.
    func sweepAbandoned(now: Date = Date()) {
        let entries = (try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: [.contentModificationDateKey]
        )) ?? []
        for entry in entries where entry.lastPathComponent.hasSuffix(Self.partialSuffix) {
            let modified = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]))?
                .contentModificationDate ?? .distantPast
            if now.timeIntervalSince(modified) > Self.abandonedAfter {
                try? FileManager.default.removeItem(at: entry)
            }
        }
    }

    // MARK: - Files

    private func read(from folder: URL) -> PendingCapture? {
        guard let data = try? Data(contentsOf: folder.appending(path: Self.recordName)),
              let capture = try? JSONDecoder().decode(PendingCapture.self, from: data),
              capture.version == PendingCapture.currentVersion,
              folder.lastPathComponent == capture.id.uuidString else { return nil }
        return capture
    }

    /// Written with complete protection on iOS: a capture is read when the
    /// user opens Sunrise, which is never while the phone is locked, so there
    /// is no reason for shared text to be readable then.
    private func write(_ capture: PendingCapture, into folder: URL) throws {
        let data = try JSONEncoder().encode(capture)
        let file = folder.appending(path: Self.recordName)
        #if os(iOS)
        try data.write(to: file, options: [.atomic, .completeFileProtection])
        #else
        try data.write(to: file, options: .atomic)
        #endif
    }
}
