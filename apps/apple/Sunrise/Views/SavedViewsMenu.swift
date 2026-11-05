import SwiftUI

/// The Views menu: recall a saved view, save the one on screen, or delete one.
struct SavedViewsMenu: View {
    let model: SavedViewsModel
    let contexts: NameBook
    let recall: (Destination) -> Void
    let saveCurrent: () -> Void

    var body: some View {
        Menu(L10n.SavedViews.menu, systemImage: "bookmark") {
            if model.views.isEmpty {
                Text(L10n.SavedViews.empty)
            } else {
                ForEach(model.views, id: \.name) { view in
                    Button {
                        recall(model.recall(view, contexts: contexts))
                    } label: {
                        // The summary is the store's — `view=search;query=…`
                        // rendered as `search · /passport · @errands` — so the
                        // app and `sunrise-cli` describe a saved view the same
                        // way.
                        Text("\(view.name) — \(view.summary)")
                    }
                }
                Divider()
                Menu(L10n.Action.delete) {
                    ForEach(model.views, id: \.name) { view in
                        Button(view.name, role: .destructive) {
                            Task { await model.delete(view) }
                        }
                    }
                }
            }
            Divider()
            Button(L10n.SavedViews.saveEllipsis, action: saveCurrent)
            if !model.warnings.isEmpty {
                Divider()
                ForEach(Array(model.warnings.enumerated()), id: \.offset) { _, warning in
                    Text(warning)
                }
            }
        }
    }
}

/// Name the view being saved.
struct SaveViewSheet: View {
    @Binding var name: String
    let summary: String
    let save: () async -> Void

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(L10n.SavedViews.sheetTitle).font(.headline)
            Text(summary).font(.caption).foregroundStyle(.secondary)
            TextField(L10n.SavedViews.name, text: $name, prompt: Text(L10n.SavedViews.namePrompt))
                // The name `sunrise` recalls this view by on the command
                // line, so a capitalised first letter is a different view.
                .textInput(.identifier)
                .onSubmit(commit)
            Text(L10n.SavedViews.footnote)
            .font(.caption2)
            .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button(L10n.Action.cancel) { dismiss() }
                Button(L10n.Action.save, action: commit)
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmed.isEmpty)
            }
        }
        .padding(16)
        .macSheetFrame(width: 360)
    }

    private func commit() {
        guard !name.trimmed.isEmpty else { return }
        Task {
            await save()
            dismiss()
        }
    }
}

/// Undo and Redo, named after what they would do.
struct UndoMenu: View {
    let model: UndoModel

    var body: some View {
        // Two buttons rather than a menu: `Cmd-Z` has to work without opening
        // anything, and the title is what says which step it means.
        Group {
            // The tooltip names the key after the step: an icon-only button
            // is where somebody hovering learns ⌘Z exists.
            Button(model.undoTitle, systemImage: AppAction.undo.symbol) {
                Task { await model.undo() }
            }
            .keyboardShortcut(for: .undo)
            .disabled(!model.canUndo)
            .labelStyle(.iconOnly)
            .help(Keymap.help(model.undoTitle, for: .undo))
            .accessibilityIdentifier("toolbar.undo")

            Button(model.redoTitle, systemImage: AppAction.redo.symbol) {
                Task { await model.redo() }
            }
            .keyboardShortcut(for: .redo)
            .disabled(!model.canRedo)
            .labelStyle(.iconOnly)
            .help(Keymap.help(model.redoTitle, for: .redo))
            .accessibilityIdentifier("toolbar.redo")
        }
    }
}
