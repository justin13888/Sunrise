import SwiftUI

/// The capture field, with the parse shown as it is typed.
///
/// The preview is not decoration. `#stream @context ^when !priority ~duration`
/// is a language, and a language with no feedback is a language people stop
/// using — so the field shows what the shared parser made of the line, and
/// says out loud when it could not place a token.
struct CaptureBar: View {
    @Bindable var model: CaptureModel
    /// The window's focus, so ⌘N can put the keyboard here from a menu item —
    /// and so `Esc` can give it back to the list. A `@FocusState` of its own
    /// would be private to this view, which is exactly the thing a shortcut
    /// needs to reach.
    var focus: FocusState<PaneFocus?>.Binding
    let commit: (TaskDraftIn) async -> Void

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
                .focused(focus, equals: .capture)
                .onSubmit(submit)
                .accessibilityIdentifier("capture.field")
                // Escape out of the field and onto the rows, so somebody who
                // hit ⌘N by mistake is one key from the list again rather than
                // reaching for the mouse.
                .onKeyPress(.escape) {
                    focus.wrappedValue = .rows
                    return .handled
                }
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
        #if os(iOS)
        // The touch analogue of the Escape binding above, and not a nicety.
        // `submit` puts focus straight back in the field so a burst of three
        // thoughts is three lines — right on a Mac, where the panel is the
        // only thing on screen that moves. On a phone the software keyboard
        // then covers the tab bar and most of the list, and until this there
        // was no affordance anywhere to put it away: no Done, no tap-outside,
        // and a hardware Escape key an iPhone does not have.
        //
        // `nil` rather than `.rows`, which is what the Escape binding above
        // uses. Escape is handing the *keyboard* to the list so that `j`, `k`
        // and `x` start meaning something, and the list has to be focused for
        // that. Here there is no cursor to hand anywhere and the whole request
        // is "put this away" — and moving focus onto a `List` does not resign
        // the field's first responder, so `.rows` left the keyboard exactly
        // where it was.
        .toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("Done") { focus.wrappedValue = nil }
                    .accessibilityIdentifier("capture.done")
            }
        }
        #endif
    }

    private func submit() {
        guard let draft = model.takeDraft() else { return }
        Task {
            await commit(draft)
            focus.wrappedValue = .capture
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
