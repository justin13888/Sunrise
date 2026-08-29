import AppKit
import SwiftUI

/// The borderless quick-capture window.
///
/// One field and the shared parser's preview, and nothing else — the whole
/// point is that it appears over whatever you were doing, takes a line, and
/// goes away. `docs/07-clients/interaction-patterns.md` makes this the persona's
/// most-used surface, which is also why the window is not sandboxed
/// (`desktop.md` §Sandboxing: a sandboxed build cannot hold a reliable
/// system-wide hotkey).
struct QuickCaptureView: View {
    @Bindable var model: CaptureModel
    /// Throwing, and that is the point. A capture the core refuses has to be
    /// visible: this panel is the fastest way in the app to record a thought,
    /// and a swallowed error made it the fastest way to lose one.
    let commit: (TaskDraftIn) async throws -> Void
    let dismiss: () -> Void

    @FocusState private var focused: Bool
    @State private var confirmation: String?
    @State private var failure: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: "sun.max")
                    .foregroundStyle(.orange)
                TextField(
                    "Capture",
                    text: $model.text,
                    prompt: Text("Renew passport #travel ^next saturday !1 ~1h")
                )
                .textFieldStyle(.plain)
                .font(.title3)
                .focused($focused)
                .onSubmit(submit)
                .accessibilityIdentifier("quick-capture.field")
            }

            if let preview = model.preview, !preview.draft.title.trimmed.isEmpty {
                Text(summary(preview))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            ForEach(Array(model.issues.enumerated()), id: \.offset) { _, issue in
                Label(issue.explanation, systemImage: "info.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            if let confirmation {
                Label(confirmation, systemImage: "checkmark.circle")
                    .font(.caption)
                    .foregroundStyle(.green)
            }
            // Not dismissed on a timer, unlike the confirmation above: this
            // one asks the user to do something — the line it names is back in
            // the field, waiting to be sent again.
            if let failure {
                Label(failure, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .accessibilityIdentifier("quick-capture.failure")
            }
        }
        .padding(16)
        .frame(width: 560)
        .background(.regularMaterial, in: .rect(cornerRadius: 14))
        .onExitCommand(perform: dismiss)
        .onAppear { focused = true }
    }

    /// Commit and stay open, so a burst of three thoughts is three lines rather
    /// than three trips to the hotkey. Escape closes it.
    ///
    /// `takeDraft()` clears the field, which is right when the write lands and
    /// wrong when it does not — so the typed line is kept here and put back on
    /// a failure. The user's words are the one thing this surface may not lose.
    private func submit() {
        let typed = model.text
        guard let draft = model.takeDraft() else { return }
        let title = draft.title
        _Concurrency.Task {
            do {
                try await commit(draft)
                failure = nil
                confirmation = "Captured “\(title)”"
                focused = true
                try? await _Concurrency.Task.sleep(for: .seconds(2))
                if confirmation?.contains(title) == true { confirmation = nil }
            } catch {
                confirmation = nil
                model.text = typed
                failure = "Not saved: \(error.localizedDescription)"
                focused = true
            }
        }
    }

    /// One line of what the parser made of it. The words are the seam's.
    private func summary(_ preview: CapturePreview) -> String {
        var parts = [preview.draft.title]
        if let priority = preview.draft.priority { parts.append("!\(priority)") }
        if let seconds = preview.draft.estimatedDurationS {
            parts.append(shortDuration(secs: seconds))
        }
        if let energy = preview.draft.energy { parts.append(energyLabel(energy: energy)) }
        let tags = preview.draft.contexts.count + (preview.draft.streamId == nil ? 0 : 1)
        if tags > 0 { parts.append("\(tags) tags") }
        return parts.joined(separator: " · ")
    }
}

/// Turns the SwiftUI quick-capture window into a floating borderless panel.
///
/// SwiftUI has no scene modifier for "no title bar, floats over full-screen
/// apps, and does not steal the whole app's activation". `NSWindow` does, and
/// this reaches it once, when the window appears.
struct FloatingPanel: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { Self.configure(view.window) }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {}

    static func configure(_ window: NSWindow?) {
        guard let window else { return }
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.standardWindowButton(.closeButton)?.isHidden = true
        window.standardWindowButton(.miniaturizeButton)?.isHidden = true
        window.standardWindowButton(.zoomButton)?.isHidden = true
        window.isMovableByWindowBackground = true
        window.backgroundColor = .clear
        window.isOpaque = false
        window.hasShadow = true
        window.level = .floating
        // Follows the user to whichever Space they are on, and appears over a
        // full-screen app — which is the only way a capture window is any use
        // to someone who was in the middle of something.
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.center()
    }
}
