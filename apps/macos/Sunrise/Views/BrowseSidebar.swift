import SwiftUI

/// The sidebar: the primary views, then the vault's own streams and contexts.
///
/// Streams and contexts are data, not navigation, so they are read from the
/// core and kept current by the change stream rather than enumerated in code.
struct BrowseSidebar: View {
    let model: BrowseModel
    @Binding var selection: Destination?

    @State private var editingStream: StreamListRow?
    @State private var editingContext: ContextListRow?
    @State private var newStream = false
    @State private var newContext = false
    @State private var confirmingStreamDelete: StreamListRow?
    @State private var confirmingContextDelete: ContextListRow?
    /// Which row a drag is currently over. `interaction-patterns.md`
    /// §Drag-and-drop UX tokens asks for a visible drop target, and a sidebar
    /// row that highlighted nothing would be a target you have to guess at.
    @State private var isTargeted: EntityRef?

    var body: some View {
        List(selection: $selection) {
            Section {
                ForEach(Destination.fixed) { destination in
                    Label(destination.title, systemImage: destination.symbol)
                        .tag(destination)
                        .accessibilityIdentifier("sidebar.\(destination.title.lowercased())")
                }
            }

            Section {
                ForEach(model.visibleStreams, id: \.id) { streamRow($0) }
            } header: {
                header("Streams", add: { newStream = true }, addLabel: "New stream")
            }

            Section {
                ForEach(model.visibleContexts, id: \.id) { contextRow($0) }
            } header: {
                header("Contexts", add: { newContext = true }, addLabel: "New context")
            }
        }
        .navigationSplitViewColumnWidth(min: 190, ideal: 220, max: 300)
        .contextMenu {
            Toggle("Show archived", isOn: Binding(
                get: { model.showsArchived },
                set: { model.showsArchived = $0 }
            ))
        }
        .task { await model.refresh() }
        .task { await model.follow() }
        .sheet(isPresented: $newStream) {
            StreamEditorView(stream: nil) { name, edit in
                await model.createStream(
                    name: name,
                    color: edit.color,
                    cadence: edit.reviewCadence
                )
            }
        }
        .sheet(isPresented: $newContext) {
            ContextEditorView(context: nil) { name, edit in
                await model.createContext(name: name, description: edit.setDescription)
            }
        }
        .sheet(item: $editingStream) { row in
            StreamEditorLoader(row: row, model: model) { edit in
                await model.updateStream(row, edit)
            }
        }
        .sheet(item: $editingContext) { row in
            ContextEditorView(context: row) { _, edit in
                await model.updateContext(row, edit)
            }
        }
        .confirmationDialog(
            "Delete “\(confirmingStreamDelete?.name ?? "")”?",
            isPresented: Binding(
                get: { confirmingStreamDelete != nil },
                set: { if !$0 { confirmingStreamDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                guard let row = confirmingStreamDelete else { return }
                confirmingStreamDelete = nil
                Task { await model.deleteStream(row) }
            }
        } message: {
            // Not a formality. `submitUndoable` reports a delete as
            // `UndoRefusal.deleted`, and the confirmation is the only place
            // that fact is useful — after the fact it is just an apology.
            Text("This cannot be undone. Archive it instead to keep its tasks reachable.")
        }
        .confirmationDialog(
            "Delete @\(confirmingContextDelete?.name ?? "")?",
            isPresented: Binding(
                get: { confirmingContextDelete != nil },
                set: { if !$0 { confirmingContextDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                guard let row = confirmingContextDelete else { return }
                confirmingContextDelete = nil
                Task { await model.deleteContext(row) }
            }
        } message: {
            Text(
                "This removes @\(confirmingContextDelete?.name ?? "") from every task "
                    + "carrying it, and cannot be undone."
            )
        }
    }

    private func header(
        _ title: String,
        add: @escaping () -> Void,
        addLabel: String
    ) -> some View {
        HStack {
            Text(title)
            Spacer()
            Button(addLabel, systemImage: "plus", action: add)
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
                .accessibilityLabel(addLabel)
        }
    }

    private func streamRow(_ row: StreamListRow) -> some View {
        Label {
            HStack {
                Text(row.name)
                    .foregroundStyle(row.archived ? .secondary : .primary)
                if row.paused {
                    Image(systemName: "pause.circle").foregroundStyle(.secondary)
                }
                Spacer()
                if row.openTaskCount > 0 {
                    Text("\(row.openTaskCount)")
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        } icon: {
            Image(systemName: "circle.fill")
                .foregroundStyle(row.color.tint)
                .font(.caption2)
        }
        .tag(Destination.list(.stream(id: row.id, name: row.name)))
        // **Task → Stream.** `interaction-patterns.md` §Promote names this
        // gesture beside the `m` key, and it runs the same command the `M`
        // sheet does.
        .dropDestination(for: String.self) { items, _ in
            Task { await model.fileTasks(items, intoStream: row.id) }
            return !DropPayload.taskIDs(items).isEmpty
        } isTargeted: { isTargeted = $0 ? row.id : (isTargeted == row.id ? nil : isTargeted) }
        .dropHighlight(isActive: isTargeted == row.id)
        .contextMenu {
            if row.id == BrowseModel.inboxID {
                // The Inbox is synthetic: there is no stream entity behind it,
                // so every one of these would be rejected by the core.
                Text("The Inbox cannot be edited")
            } else {
                Button("Edit…") { editingStream = row }
                Button(row.paused ? "Resume" : "Pause") {
                    Task { await model.setStreamPaused(row, !row.paused) }
                }
                Button(row.archived ? "Unarchive" : "Archive") {
                    Task { await model.setStreamArchived(row, !row.archived) }
                }
                Divider()
                Button("Delete…", role: .destructive) { confirmingStreamDelete = row }
            }
        }
    }

    private func contextRow(_ row: ContextListRow) -> some View {
        HStack {
            Text("@\(row.name)")
                .foregroundStyle(row.archived ? .secondary : .primary)
            Spacer()
            if row.taskCount > 0 {
                Text("\(row.taskCount)")
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
        .tag(Destination.list(.context(id: row.id, name: row.name)))
        // **Task → Context.** Adds rather than replaces: a task has one stream
        // and any number of contexts, so dropping `@home` on it must not take
        // `@errands` away.
        .dropDestination(for: String.self) { items, _ in
            Task { await model.fileTasks(items, intoContext: row.id) }
            return !DropPayload.taskIDs(items).isEmpty
        } isTargeted: { isTargeted = $0 ? row.id : (isTargeted == row.id ? nil : isTargeted) }
        .dropHighlight(isActive: isTargeted == row.id)
        .contextMenu {
            Button("Edit…") { editingContext = row }
            Button(row.archived ? "Unarchive" : "Archive") {
                Task { await model.setContextArchived(row, !row.archived) }
            }
            Divider()
            Button("Delete…", role: .destructive) { confirmingContextDelete = row }
        }
    }
}

/// `sheet(item:)` and `ForEach` want an `Identifiable`; both rows already have
/// the id.
extension StreamListRow: Identifiable {}
extension ContextListRow: Identifiable {}

extension StreamColor {
    /// The swatch for a stream's colour.
    ///
    /// The *names* are the domain's — `slate`, `rose`, `emerald` — and this
    /// only decides what each one looks like on this platform, which is a
    /// rendering choice and nothing more.
    var tint: Color {
        switch self {
        case .slate: .gray
        case .rose: .pink
        case .amber: .orange
        case .emerald: .green
        case .sky: .cyan
        case .indigo: .indigo
        case .violet: .purple
        case .pink: Color(red: 0.95, green: 0.45, blue: 0.7)
        }
    }

    /// What the picker calls it.
    var label: String {
        switch self {
        case .slate: "Slate"
        case .rose: "Rose"
        case .amber: "Amber"
        case .emerald: "Emerald"
        case .sky: "Sky"
        case .indigo: "Indigo"
        case .violet: "Violet"
        case .pink: "Pink"
        }
    }

    static let all: [StreamColor] = [
        .slate, .rose, .amber, .emerald, .sky, .indigo, .violet, .pink
    ]
}
