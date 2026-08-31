import AppKit
import SwiftUI

/// A task's note body: read it, or edit it a block at a time.
///
/// Schema-locked to the grammar in `docs/02-domain/notes.md`. The block menu
/// offers exactly the kinds that grammar names, the mark buttons offer
/// exactly its five marks, and everything is written back through the core's
/// encoder — so there is no path from this screen to a body the format cannot
/// express.
struct NoteBodyEditor: View {
    @Bindable var model: NoteEditorModel

    @FocusState private var focused: UUID?

    var body: some View {
        VStack(spacing: 0) {
            if let lock = model.lock {
                NoteLockBanner(message: lock.message)
            } else {
                toolbar
            }
            Divider()
            content
            if let warning = model.lengthWarning {
                Divider()
                Label(warning, systemImage: "exclamationmark.circle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
            }
        }
        .onChange(of: focused) { _, new in
            model.focusedBlock = new.flatMap(model.owningBlock(of:))
        }
    }

    private var content: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 8) {
                ForEach($model.blocks) { $block in
                    NoteBlockRow(
                        block: $block,
                        model: model,
                        focused: $focused
                    )
                }
            }
            .padding(12)
        }
        .frame(minHeight: 260)
    }

    // MARK: - Toolbar

    private var toolbar: some View {
        HStack(spacing: 6) {
            ForEach(NoteMark.all, id: \.self) { mark in
                Button(mark.label, systemImage: mark.symbol) {
                    model.toggleMark(mark)
                }
                .keyboardShortcut(mark.shortcut, modifiers: .command)
                .buttonStyle(.borderless)
                .labelStyle(.iconOnly)
                .foregroundStyle(model.isActive(mark) ? Color.accentColor : .primary)
                .help(mark.label)
            }

            Divider().frame(height: 14)

            Menu {
                ForEach(NoteBlockKind.allCases) { kind in
                    Button(kind.title, systemImage: kind.symbol) {
                        model.insertBlock(kind, after: model.focusedBlock)
                    }
                }
            } label: {
                Label("Add block", systemImage: "plus")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Add a block below")

            Spacer()

            Button("Copy as Markdown", systemImage: "doc.on.doc") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(model.markdown, forType: .string)
            }
            .buttonStyle(.borderless)
            .labelStyle(.iconOnly)
            .help("Copy as Markdown")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

/// The sentence that says why a note is being shown rather than edited.
private struct NoteLockBanner: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "lock")
            .font(.caption)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
    }
}

// ---------------------------------------------------------------------------
// One block
// ---------------------------------------------------------------------------

/// One block, with the menu that changes what it is.
private struct NoteBlockRow: View {
    @Binding var block: NoteEditorBlock
    let model: NoteEditorModel
    @FocusState.Binding var focused: UUID?

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            if !model.isReadOnly { gutter }
            body(for: block.kind)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// One menu rather than a row of buttons: at this width a block cannot
    /// afford a kind picker *and* move and delete controls, and all four are
    /// the same question — what should this block be, and where.
    private var gutter: some View {
        Menu {
            Picker("Block", selection: kindBinding) {
                ForEach(NoteBlockKind.allCases) { kind in
                    Label(kind.title, systemImage: kind.symbol).tag(kind)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Button("Move up", systemImage: "arrow.up") { model.moveBlock(block.id, by: -1) }
            Button("Move down", systemImage: "arrow.down") { model.moveBlock(block.id, by: 1) }
            Divider()
            Button("Delete block", systemImage: "trash", role: .destructive) {
                model.removeBlock(block.id)
            }
        } label: {
            Image(systemName: block.kind.symbol)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .frame(width: 22)
        .foregroundStyle(.secondary)
        .help(block.kind.title)
    }

    private var kindBinding: Binding<NoteBlockKind> {
        Binding(
            get: { block.kind },
            set: { model.setKind($0, for: block.id) }
        )
    }

    @ViewBuilder
    private func body(for kind: NoteBlockKind) -> some View {
        switch kind {
        case .divider:
            Divider().padding(.vertical, 6)
        case .code:
            codeBody
        case .bulleted, .numbered, .checklist:
            rowsBody
        default:
            textBody
        }
    }

    // MARK: - Text

    @ViewBuilder
    private var textBody: some View {
        if model.isReadOnly {
            Text(block.text)
                .font(font(for: block.kind))
                .textSelection(.enabled)
                .modifier(QuoteRule(active: block.kind == .quote))
        } else {
            TextEditor(text: $block.text, selection: $block.selection)
                .font(font(for: block.kind))
                .frame(minHeight: minHeight(for: block.kind))
                .scrollContentBackground(.hidden)
                .scrollDisabled(true)
                .focused($focused, equals: block.id)
                .modifier(QuoteRule(active: block.kind == .quote))
        }
    }

    private func font(for kind: NoteBlockKind) -> Font {
        switch kind {
        case .heading1: .title2.bold()
        case .heading2: .title3.bold()
        case .heading3: .headline
        case .quote: .body.italic()
        default: .body
        }
    }

    private func minHeight(for kind: NoteBlockKind) -> CGFloat {
        switch kind {
        case .heading1: 30
        case .heading2, .heading3: 26
        default: 22
        }
    }

    // MARK: - Code

    @ViewBuilder
    private var codeBody: some View {
        VStack(alignment: .leading, spacing: 4) {
            if model.isReadOnly {
                Text(block.code)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                TextField("Language", text: $block.language, prompt: Text("swift"))
                    .font(.caption)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 120)
                TextEditor(text: $block.code)
                    .font(.system(.body, design: .monospaced))
                    .frame(minHeight: 64)
                    .scrollContentBackground(.hidden)
                    .focused($focused, equals: block.id)
            }
        }
        .padding(8)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 6))
    }

    // MARK: - Rows

    private var rowsBody: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(Array($block.rows.enumerated()), id: \.element.id) { index, $row in
                NoteRowLine(
                    row: $row,
                    ordinal: index + 1,
                    block: block,
                    model: model,
                    focused: $focused
                )
            }
            if !model.isReadOnly {
                Button("Add row", systemImage: "plus") { model.addRow(to: block.id) }
                    .buttonStyle(.borderless)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

/// One row of a list or checklist.
private struct NoteRowLine: View {
    @Binding var row: NoteEditorRow
    let ordinal: Int
    let block: NoteEditorBlock
    let model: NoteEditorModel
    @FocusState.Binding var focused: UUID?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            marker
            if model.isReadOnly {
                Text(row.text).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                TextEditor(text: $row.text, selection: $row.selection)
                    .frame(minHeight: 22)
                    .scrollContentBackground(.hidden)
                    .scrollDisabled(true)
                    .focused($focused, equals: row.id)
                controls
            }
        }
        .padding(.leading, CGFloat(row.depth) * 16)
    }

    @ViewBuilder
    private var marker: some View {
        switch block.kind {
        case .checklist:
            Toggle("Done", isOn: $row.checked)
                .labelsHidden()
                .toggleStyle(.checkbox)
                .disabled(model.isReadOnly)
        case .numbered:
            Text("\(ordinal).")
                .font(.body.monospacedDigit())
                .foregroundStyle(.secondary)
        default:
            Text("•").foregroundStyle(.secondary)
        }
    }

    private var controls: some View {
        Menu {
            if block.kind.allowsNesting {
                Button("Indent", systemImage: "increase.indent") {
                    model.indentRow(row.id, in: block.id)
                }
                Button("Outdent", systemImage: "decrease.indent") {
                    model.outdentRow(row.id, in: block.id)
                }
                Divider()
            }
            Button("Add row below", systemImage: "plus") {
                model.addRow(to: block.id, after: row.id)
            }
            Button("Delete row", systemImage: "trash", role: .destructive) {
                model.removeRow(row.id, from: block.id)
            }
        } label: {
            Image(systemName: "ellipsis")
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .frame(width: 18)
        .foregroundStyle(.tertiary)
    }
}

/// The rule down the left of a quote.
private struct QuoteRule: ViewModifier {
    let active: Bool

    func body(content: Content) -> some View {
        if active {
            content
                .padding(.leading, 8)
                .overlay(alignment: .leading) {
                    Rectangle().fill(.quaternary).frame(width: 3)
                }
        } else {
            content
        }
    }
}
