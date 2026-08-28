import SwiftUI

/// The Views menu: recall a saved view, save the one on screen, or delete one.
struct SavedViewsMenu: View {
    let model: SavedViewsModel
    let contexts: NameBook
    let recall: (Destination) -> Void
    let saveCurrent: () -> Void

    var body: some View {
        Menu("Views", systemImage: "bookmark") {
            if model.views.isEmpty {
                Text("No saved views")
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
                Menu("Delete") {
                    ForEach(model.views, id: \.name) { view in
                        Button(view.name, role: .destructive) {
                            Task { await model.delete(view) }
                        }
                    }
                }
            }
            Divider()
            Button("Save this view…", action: saveCurrent)
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
            Text("Save this view").font(.headline)
            Text(summary).font(.caption).foregroundStyle(.secondary)
            TextField("Name", text: $name, prompt: Text("errands"))
                .onSubmit(commit)
            Text(
                "Saved to the same file `sunrise` reads, so this view is recallable "
                    + "from the command line too."
            )
            .font(.caption2)
            .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save", action: commit)
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmed.isEmpty)
            }
        }
        .padding(16)
        .frame(width: 360)
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
            Button(model.undoTitle, systemImage: "arrow.uturn.backward") {
                Task { await model.undo() }
            }
            .keyboardShortcut("z", modifiers: .command)
            .disabled(!model.canUndo)
            .labelStyle(.iconOnly)
            .help(model.undoTitle)

            Button(model.redoTitle, systemImage: "arrow.uturn.forward") {
                Task { await model.redo() }
            }
            .keyboardShortcut("z", modifiers: [.command, .shift])
            .disabled(!model.canRedo)
            .labelStyle(.iconOnly)
            .help(model.redoTitle)
        }
    }
}
