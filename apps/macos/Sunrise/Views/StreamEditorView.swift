import SwiftUI

/// Create or edit a stream.
///
/// One sheet for both, because the fields are the same and a second sheet
/// would be a second place for them to drift apart. `stream == nil` is a
/// creation.
struct StreamEditorView: View {
    let stream: StreamListRow?
    /// The name, and the edit that carries everything else. A creation reads
    /// the name and the colour off it; an edit submits it whole.
    let commit: (String, StreamEdit) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var name: String
    @State private var color: StreamColor
    @State private var cadence: StreamReviewCadence

    init(stream: StreamListRow?, commit: @escaping (String, StreamEdit) async -> Void) {
        self.stream = stream
        self.commit = commit
        _name = State(initialValue: stream?.name ?? "")
        _color = State(initialValue: stream?.color ?? .slate)
        // `StreamListRow` carries no cadence — the sidebar has no use for one
        // — so an edit starts from the default rather than from a read the
        // list did not make. Changing it is opt-in; leaving it alone submits
        // no cadence at all.
        _cadence = State(initialValue: .none)
    }

    private var isCreating: Bool { stream == nil }

    var body: some View {
        Form {
            TextField("Name", text: $name)

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
