import Foundation

/// The capture field's live preview.
///
/// Debounced, because a preview costs two vault reads to resolve `#stream` and
/// `@context` and a text field fires on every keystroke.
@MainActor
@Observable
final class CaptureModel {
    var text: String = "" {
        didSet { schedulePreview() }
    }

    private(set) var preview: CapturePreview?

    /// Whether the line is worth committing. An empty title is rejected by the
    /// core's own validation, so the button is disabled rather than the error
    /// shown.
    var canCommit: Bool {
        !(preview?.draft.title.trimmed.isEmpty ?? true)
    }

    /// Tokens the parser could not apply. Their text is still in the title, so
    /// this is an explanation and not a warning.
    var issues: [CaptureIssue] { preview?.issues ?? [] }

    private let bridge: CoreBridge
    private let debounce: Duration
    private var pending: Task<Void, Never>?

    init(bridge: CoreBridge, debounce: Duration = .milliseconds(120)) {
        self.bridge = bridge
        self.debounce = debounce
    }

    /// Take the parsed draft and clear the field.
    func takeDraft() -> TaskDraftIn? {
        guard let draft = preview?.draft, !draft.title.trimmed.isEmpty else { return nil }
        pending?.cancel()
        text = ""
        preview = nil
        return draft
    }

    func clear() {
        pending?.cancel()
        text = ""
        preview = nil
    }

    private func schedulePreview() {
        pending?.cancel()
        let line = text
        guard !line.trimmed.isEmpty else {
            preview = nil
            return
        }
        pending = Task { [debounce, bridge] in
            try? await Task.sleep(for: debounce)
            guard !Task.isCancelled else { return }
            let parsed = try? await bridge.previewCapture(line, timeZone: TimeZone.current.identifier)
            guard !Task.isCancelled else { return }
            // A late answer for a line the user has already moved on from must
            // not overwrite a newer one.
            if line == text { preview = parsed }
        }
    }
}

extension CaptureIssue {
    /// What to tell the user. Every case keeps the typed text, because the
    /// point is that nothing was lost — the token is still in the title.
    var explanation: String {
        switch self {
        case let .unknownStream(name):
            "No stream called “\(name)” — kept in the title."
        case let .ambiguousStream(typed, candidates):
            "“\(typed)” matches \(candidates.joined(separator: ", "))."
        case let .unknownContext(name):
            "No context called “\(name)” — kept in the title."
        case let .ambiguousContext(typed, candidates):
            "“\(typed)” matches \(candidates.joined(separator: ", "))."
        case let .unparseableDate(text):
            "“\(text)” is not a date Sunrise understands."
        case let .priorityOutOfRange(text):
            "Priority “\(text)” is outside 1–5."
        case let .unparseableDuration(text):
            "“\(text)” is not a duration."
        }
    }
}
