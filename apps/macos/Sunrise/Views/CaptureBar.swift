import SwiftUI

/// The capture field, with the parse shown as it is typed.
///
/// The preview is not decoration. `#stream @context ^when !priority ~duration`
/// is a language, and a language with no feedback is a language people stop
/// using — so the field shows what the shared parser made of the line, and
/// says out loud when it could not place a token.
struct CaptureBar: View {
    @Bindable var model: CaptureModel
    let commit: (TaskDraftIn) async -> Void

    @FocusState private var isFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Image(systemName: "plus.circle")
                    .foregroundStyle(.secondary)
                TextField(
                    "Capture",
                    text: $model.text,
                    prompt: Text("Renew passport #travel ^next saturday !1 ~1h")
                )
                .textFieldStyle(.plain)
                .focused($isFocused)
                .onSubmit(submit)
                .accessibilityIdentifier("capture.field")
                Button("Add", action: submit)
                    .accessibilityIdentifier("capture.add")
                    .buttonStyle(.borderedProminent)
                    .disabled(!model.canCommit)
                    .keyboardShortcut(.return, modifiers: [])
            }
            .padding(10)
            .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 8))

            if let preview = model.preview {
                previewLine(preview)
            }
            ForEach(Array(model.issues.enumerated()), id: \.offset) { _, issue in
                Label(issue.explanation, systemImage: "info.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 10)
    }

    private func submit() {
        guard let draft = model.takeDraft() else { return }
        Task {
            await commit(draft)
            isFocused = true
        }
    }

    @ViewBuilder
    private func previewLine(_ preview: CapturePreview) -> some View {
        let draft = preview.draft
        HStack(spacing: 8) {
            Text(draft.title.isEmpty ? "—" : draft.title)
                .font(.caption.weight(.medium))
            if let priority = draft.priority {
                Text("!\(priority)").font(.caption).foregroundStyle(.secondary)
            }
            if let seconds = draft.estimatedDurationS {
                Text(shortDuration(secs: seconds)).font(.caption).foregroundStyle(.secondary)
            }
            if let energy = draft.energy {
                Text(energyLabel(energy: energy)).font(.caption).foregroundStyle(.secondary)
            }
            if draft.streamId != nil || !draft.contexts.isEmpty {
                Text("\(draft.contexts.count + (draft.streamId == nil ? 0 : 1)) tags")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.leading, 4)
    }
}
