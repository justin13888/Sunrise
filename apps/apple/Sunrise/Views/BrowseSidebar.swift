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
                        .selectableOnTouch(destination, selection: $selection)
                }
            }

            Section {
                // The Inbox sits outside the `ForEach` because it is outside
                // the *order*: it is synthetic, the core pins it to the top,
                // and `.onMove` below must not be able to pick it up or drop
                // anything above it.
                if let inbox = model.inboxStream { streamRow(inbox) }
                ForEach(model.orderableStreams, id: \.id) { streamRow($0) }
                    // **Reorder.** `interaction-patterns.md` §Reorder asks for
                    // drag-within-a-list, and for streams the answer is a
                    // vault fact: this writes `Stream.sort_order` through the
                    // core, so the arrangement syncs. Task lists still keep
                    // theirs per device — see `ListOrderStore`.
                    .onMove { source, destination in
                        Task { await model.moveStreams(from: source, to: destination) }
                    }
            } header: {
                header("Streams")
            }

            Section {
                ForEach(model.visibleContexts, id: \.id) { contextRow($0) }
            } header: {
                header("Contexts")
            }
        }
        .navigationSplitViewColumnWidth(min: 190, ideal: 220, max: 300)
        // The add controls, placed where each platform puts them — and out of
        // the section headers they used to live in on both, because a header
        // cannot hold a control that accessibility can see. See `header(_:)`.
        //
        // On the Mac this is a bottom bar under the sidebar, which is where
        // Mail, Reminders and Finder's tags all keep their `+`. On iPhone that
        // space belongs to the tab bar — a bar of its own there overlaps both
        // the tab bar and the last rows of the list — so the same two actions
        // become a toolbar menu, which is where iOS puts them.
        //
        // Only the iOS half of this is proved in CI. `SidebarAddButtonTests`
        // runs on the simulator on every `mise run ios-app`; the macOS suite is
        // `skipped: true` in the `Sunrise` scheme, because a macOS XCUITest
        // needs two one-time grants to the machine — developer mode, and an
        // automation grant keyed to the app bundle's path — that a CI runner
        // cannot give. See `macos-uitest` in `mise.toml`.
        //
        // The gap is narrower than it sounds. The two branches differ in
        // placement only, and what the simulator proves — both actions in the
        // accessibility tree under their own names, and not merely under their
        // identifiers — is the claim that was broken. The Mac's bottom bar is
        // covered for layout by `mise run apple-shots` on a developer machine,
        // which is where those grants live.
        //
        // A cheaper macOS test hosting this view in an `NSHostingView` would
        // not close the gap: that materialises a different accessibility tree
        // from the windowed app the bug lived in, and would have been green
        // against the original defect.
        #if os(macOS)
        .safeAreaInset(edge: .bottom) {
            HStack(spacing: 4) {
                addButton("New stream", systemImage: "plus",
                          identifier: "sidebar.stream.new") { newStream = true }
                addButton("New context", systemImage: "at",
                          identifier: "sidebar.context.new") { newContext = true }
                Spacer()
            }
            .buttonStyle(.borderless)
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
        #else
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Menu("Add", systemImage: "plus") {
                    Button("New stream", systemImage: "plus") { newStream = true }
                        .accessibilityIdentifier("sidebar.stream.new")
                    Button("New context", systemImage: "at") { newContext = true }
                        .accessibilityIdentifier("sidebar.context.new")
                }
                .accessibilityLabel("Add")
                .accessibilityIdentifier("sidebar.add")
            }
        }
        #endif
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

    /// One of the two add buttons under the sidebar.
    ///
    /// `.labelStyle(.iconOnly)` sits *inside* `.accessibilityLabel` rather
    /// than on the enclosing `HStack`, which is the natural place to put it.
    /// An icon-only label leaves nothing on screen to read, so the label has
    /// to be stated for accessibility explicitly, and stating it outside the
    /// style is the order that survives.
    ///
    /// `SidebarAddButtonTests` asserts the labels reach the accessibility tree
    /// by looking them up by name rather than by identifier — the two are
    /// separate attributes, and a control findable only by identifier is
    /// findable by a test and silent to VoiceOver.
    private func addButton(
        _ title: String,
        systemImage: String,
        identifier: String,
        action: @escaping () -> Void
    ) -> some View {
        Button(title, systemImage: systemImage, action: action)
            .labelStyle(.iconOnly)
            .accessibilityLabel(title)
            .accessibilityIdentifier(identifier)
    }

    /// A section title.
    ///
    /// It holds no button, and that is the fix rather than a simplification.
    /// A `Button` inside a `List` section header on macOS 26 is **absent from
    /// the accessibility tree entirely** — measured against the running app,
    /// not assumed. The `+` rendered and worked under a mouse, and the row it
    /// lived in exposed exactly one element: an `AXHeading`. No `AXButton`,
    /// at any button style, with or without
    /// `.accessibilityElement(children: .contain)`. Both were tried and both
    /// changed nothing. That is why `testCreatingAStreamFromTheSidebar` was
    /// red: there was no button to find.
    ///
    /// The add controls moved to the bottom bar, which is where macOS puts
    /// them anyway — Mail, Reminders and Finder's tags all carry a `+` under
    /// the sidebar rather than in it.
    ///
    /// The heading the row does expose carries no name of its own. That is a
    /// smaller problem than an unreachable control — a heading with no name is
    /// skipped, not mis-actioned — and it is recorded in #38 rather than
    /// worked around with a fake row.
    private func header(_ title: String) -> some View {
        Text(title)
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
        .accessibilityIdentifier("sidebar.stream.\(row.name.lowercased())")
        .selectableOnTouch(
            Destination.list(.stream(id: row.id, name: row.name)),
            selection: $selection
        )
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
        .selectableOnTouch(
            Destination.list(.context(id: row.id, name: row.name)),
            selection: $selection
        )
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
    // `tint` moved to `Sunrise/Design/Tokens.swift`. It was eight system-colour
    // aliases plus one raw `Color(red:green:blue:)`, which made this the only
    // place in the Apple app that decided a stream's colour — and made it
    // decide a different palette from the one `packages/sunrise-ui` gave the
    // web app under the same eight names. It is now the generated token set.

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
