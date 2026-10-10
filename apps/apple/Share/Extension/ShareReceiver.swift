import Foundation
import UniformTypeIdentifiers

/// Turning what the share sheet handed over into a ``PendingCapture``.
///
/// Text, a web link and up to ``imageLimit`` images, from however many items
/// the sharing app sent. A browser sends a page as a link plus its title as the
/// item's content text, and both are kept.
enum ShareReceiver {
    /// The most images one share files. `project.yml`'s activation rule
    /// offers Sunrise for no more than this, so the sheet never shows it for a
    /// share it would cut short.
    static let imageLimit = 10

    /// The core's attachment ceiling (`MAX_ATTACHMENT_BYTES`). An image past
    /// it is refused here, while the user is still looking, rather than by the
    /// core on the next open, where it would never succeed.
    static let maxImageBytes = 100 * 1024 * 1024

    enum Failure: Error, LocalizedError {
        case noAppGroup
        case nothingUsable
        case imageTooLarge(String)

        var errorDescription: String? {
            switch self {
            case .noAppGroup:
                "Sunrise could not reach its shared storage on this device."
            case .nothingUsable:
                "There was no text, link or image to add."
            case let .imageTooLarge(name):
                "“\(name)” is larger than the 100 MB an attachment can be."
            }
        }
    }

    /// Read every item, copy its images into the store, and publish the
    /// capture. Nothing is published unless everything was read.
    @MainActor
    static func save(_ items: [NSExtensionItem], to store: PendingCaptureStore) async throws {
        let id = UUID()
        let partial = try store.begin(id)
        do {
            var capture = PendingCapture(
                id: id,
                createdAtMs: Int64(Date().timeIntervalSince1970 * 1000)
            )
            var texts: [String] = []
            for item in items {
                if let content = item.attributedContentText?.string, !content.isEmpty {
                    texts.append(content)
                }
                for provider in item.attachments ?? [] {
                    try await read(provider, into: &capture, texts: &texts, partial: partial)
                }
            }
            capture.text = joined(texts)
            guard !capture.isEmpty else { throw Failure.nothingUsable }
            try store.commit(capture, from: partial)
        } catch {
            store.discard(partial: partial)
            throw error
        }
    }

    /// The distinct texts, in order. A browser can send the page title both
    /// as the item's content text and as a text attachment.
    static func joined(_ texts: [String]) -> String? {
        var seen = Set<String>()
        let distinct = texts
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty && seen.insert($0).inserted }
        return distinct.isEmpty ? nil : distinct.joined(separator: "\n")
    }

    @MainActor
    private static func read(
        _ provider: NSItemProvider,
        into capture: inout PendingCapture,
        texts: inout [String],
        partial: URL
    ) async throws {
        if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) {
            guard capture.images.count < imageLimit else { return }
            let index = capture.images.count
            capture.images.append(try await copyImage(from: provider, index: index, into: partial))
        } else if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier) {
            if let url = try await load(URL.self, .url, from: provider), !url.isFileURL, capture.url == nil {
                capture.url = url
            }
        } else if provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) {
            if let text = try await load(String.self, .plainText, from: provider) {
                texts.append(text)
            }
        }
    }

    /// One item, as a value that can leave the provider's callback.
    ///
    /// The callback form rather than the `async` one, so that only the
    /// converted value — a `URL` or a `String` — crosses back, and never the
    /// provider's own `NSSecureCoding` object.
    @MainActor
    private static func load<Value: Sendable>(
        _: Value.Type,
        _ type: UTType,
        from provider: NSItemProvider
    ) async throws -> Value? {
        try await withCheckedThrowingContinuation { continuation in
            provider.loadItem(forTypeIdentifier: type.identifier, options: nil) { item, error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume(returning: item as? Value)
                }
            }
        }
    }

    /// Copy one image into the capture's directory.
    ///
    /// Copied inside the callback because the file the provider hands over is
    /// deleted the moment the callback returns.
    @MainActor
    private static func copyImage(
        from provider: NSItemProvider,
        index: Int,
        into partial: URL
    ) async throws -> PendingCapture.Image {
        let suggested = provider.suggestedName
        return try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadFileRepresentation(forTypeIdentifier: UTType.image.identifier) { url, error in
                guard let url else {
                    continuation.resume(throwing: error ?? Failure.nothingUsable)
                    return
                }
                do {
                    let ext = url.pathExtension.isEmpty ? "jpg" : url.pathExtension
                    let base = suggested.map { ($0 as NSString).deletingPathExtension }
                        ?? "Image \(index + 1)"
                    let name = "\(base).\(ext)"
                    let size = (try url.resourceValues(forKeys: [.fileSizeKey])).fileSize ?? 0
                    guard size <= maxImageBytes else { throw Failure.imageTooLarge(name) }
                    let file = "image-\(index).\(ext)"
                    let copy = partial.appending(path: file)
                    try FileManager.default.copyItem(at: url, to: copy)
                    try FileManager.default.setAttributes(
                        [.protectionKey: FileProtectionType.complete],
                        ofItemAtPath: copy.path
                    )
                    continuation.resume(returning: PendingCapture.Image(file: file, name: name))
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }
}
