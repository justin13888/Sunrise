import SwiftUI

/// Create or edit a stream.
///
/// One sheet for both, because the fields are the same and a second sheet
/// would be a second place for them to drift apart. `stream == nil` is a
/// creation.
///
/// It takes the **whole** `StreamItem`, not the sidebar's `StreamListRow`.
/// The row carries only what a sidebar shows — no review cadence — and a form
/// that submitted a field it had never read would quietly reset it every time
/// someone renamed a stream. One query is cheaper than that bug.
struct StreamEditorView: View {
    let stream: StreamItem?
    /// The name, and the edit that carries everything else. A creation reads
    /// the name, the colour and the cadence off it; an edit submits it whole.
    let commit: (String, StreamEdit) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var name: String
    @State private var color: StreamColor
    @State private var cadence: StreamReviewCadence

    init(stream: StreamItem?, commit: @escaping (String, StreamEdit) async -> Void) {
        self.stream = stream
        self.commit = commit
        _name = State(initialValue: stream?.name ?? "")
        _color = State(initialValue: stream?.color ?? .slate)
        _cadence = State(initialValue: stream?.reviewCadence ?? .none)
    }

    private var isCreating: Bool { stream == nil }

    var body: some View {
        Form {
            // Identified because it is not the only text field on screen when
            // this sheet opens: the list behind it has a capture bar, and a UI
            // test reaching for "the first text field" was as likely to type
            // the stream's name into a task.
            TextField("Name", text: $name)
                .accessibilityIdentifier("stream.name")

            Picker("Colour", selection: $color) {
                ForEach(StreamColor.all, id: \.self) { option in
                    Label {
                        Text(option.label)
                    } icon: {
                        Image(systemName: "circle.fill").foregroundStyle(option.tint)
                    }
                    .tag(option)
                }
            }

            Picker("Review cadence", selection: $cadence) {
                Text("None").tag(StreamReviewCadence.none)
                Text("Weekly").tag(StreamReviewCadence.weekly)
                Text("Biweekly").tag(StreamReviewCadence.biweekly)
                Text("Monthly").tag(StreamReviewCadence.monthly)
            }

            Section {
                HStack {
                    Spacer()
                    Button("Cancel") { dismiss() }
                    Button(isCreating ? "Create" : "Save") {
                        Task {
                            await commit(name.trimmed, edit)
                            dismiss()
                        }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmed.isEmpty)
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 380)
        .padding(.vertical, 8)
        .navigationTitle(isCreating ? "New stream" : "Edit stream")
    }

    private var edit: StreamEdit {
        var edit = StreamEdit()
        edit.name = name.trimmed
        edit.color = color
        edit.reviewCadence = cadence
        return edit
    }
}

/// Loads the whole stream, then edits it.
///
/// The sheet is presented from a sidebar row, which is not enough to edit
/// with; this waits for the read rather than opening a form pre-filled with
/// defaults the user never chose.
struct StreamEditorLoader: View {
    let row: StreamListRow
    let model: BrowseModel
    let commit: (StreamEdit) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var stream: StreamItem?
    @State private var failed = false

    var body: some View {
        Group {
            if let stream {
                StreamEditorView(stream: stream) { _, edit in await commit(edit) }
            } else if failed {
                VStack(spacing: 12) {
                    Text("“\(row.name)” could not be read.")
                    Button("Close") { dismiss() }
                }
                .padding(24)
                .frame(width: 320)
            } else {
                ProgressView().padding(40).frame(width: 320)
            }
        }
        .task {
            stream = await model.stream(row.id)
            failed = stream == nil
        }
    }
}

/// Create or edit a context.
struct ContextEditorView: View {
    let context: ContextListRow?
    let commit: (String, ContextEdit) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var name: String
    @State private var description: String

    init(context: ContextListRow?, commit: @escaping (String, ContextEdit) async -> Void) {
        self.context = context
        self.commit = commit
        _name = State(initialValue: context?.name ?? "")
        _description = State(initialValue: context?.description ?? "")
    }

    private var isCreating: Bool { context == nil }

    var body: some View {
        Form {
            TextField("Name", text: $name, prompt: Text("errands"))
            TextField("Description", text: $description, prompt: Text("Optional"))

            Section {
                HStack {
                    Spacer()
                    Button("Cancel") { dismiss() }
                    Button(isCreating ? "Create" : "Save") {
                        Task {
                            await commit(name.trimmed, edit)
                            dismiss()
                        }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmed.isEmpty)
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 380)
        .padding(.vertical, 8)
        .navigationTitle(isCreating ? "New context" : "Edit context")
    }

    /// Split-optional: an emptied description asks for the field to be
    /// cleared, which is a different request from leaving it alone.
    private var edit: ContextEdit {
        var edit = ContextEdit()
        edit.name = name.trimmed
        if let text = description.nilIfBlank {
            edit.setDescription = text
        } else {
            edit.clearDescription = true
        }
        return edit
    }
}
