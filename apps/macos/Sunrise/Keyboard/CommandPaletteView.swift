import SwiftUI

/// ⌘⇧P: type a command's name, press Return.
///
/// Also the app's shortcut reference-of-record. Every row prints its binding,
/// which is what `docs/08-features/keyboard.md` asks the palette to do and what
/// makes the keymap discoverable without a manual.
struct CommandPaletteView: View {
    @Bindable var model: CommandPaletteModel
    let run: (AppAction) -> Void

    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            field
            Divider()
            rows
        }
        .frame(width: 460, height: 380)
        .task { focused = true }
    }

    private var field: some View {
        HStack(spacing: 8) {
            Image(systemName: "command")
                .foregroundStyle(.secondary)
            TextField("Command", text: $model.query, prompt: Text("Run a command"))
                .textFieldStyle(.plain)
                .font(.title3)
                .focused($focused)
                .accessibilityIdentifier("palette.field")
                .accessibilityLabel("Run a command")
                .onSubmit(commit)
                // Arrow keys belong to the result list even while the field has
                // the keyboard — which it always does, because the field is the
                // only thing anybody types into here.
                .onKeyPress(.upArrow) {
                    model.moveHighlight(.up)
                    return .handled
                }
                .onKeyPress(.downArrow) {
                    model.moveHighlight(.down)
                    return .handled
                }
        }
        .padding(12)
    }

    @ViewBuilder
    private var rows: some View {
        let results = model.results
        if results.isEmpty {
            ContentUnavailableView(
                "No command matches",
                systemImage: "magnifyingglass",
                description: Text("Try a shorter word.")
            )
        } else {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(Array(results.enumerated()), id: \.element.id) { index, entry in
                            row(entry, isHighlighted: index == model.highlighted)
                                .id(entry.id)
                        }
                    }
                    .padding(6)
                }
                .onChange(of: model.highlighted) { _, index in
                    guard results.indices.contains(index) else { return }
                    withAnimation(.linear(duration: 0.1)) {
                        proxy.scrollTo(results[index].id, anchor: .center)
                    }
                }
            }
        }
    }

    private func row(_ entry: PaletteEntry, isHighlighted: Bool) -> some View {
        Button {
            guard entry.isEnabled else { return }
            run(entry.action)
        } label: {
            HStack(spacing: 10) {
                Image(systemName: entry.symbol)
                    .frame(width: 18)
                    .foregroundStyle(entry.isEnabled ? .primary : .tertiary)
                Text(entry.title)
                    .foregroundStyle(entry.isEnabled ? .primary : .tertiary)
                Spacer(minLength: 12)
                if !entry.shortcut.isEmpty {
                    Text(entry.shortcut)
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .contentShape(.rect)
            .background(
                isHighlighted ? Color.accentColor.opacity(0.18) : .clear,
                in: .rect(cornerRadius: 6)
            )
        }
        .buttonStyle(.plain)
        .disabled(!entry.isEnabled)
        // VoiceOver reads the binding as part of the row rather than as a
        // second, unlabelled string beside it.
        .accessibilityLabel(
            entry.shortcut.isEmpty ? entry.title : "\(entry.title), \(entry.shortcut)"
        )
        .accessibilityAddTraits(isHighlighted ? [.isSelected] : [])
        .accessibilityHint(entry.isEnabled ? "" : "Needs a selected task")
    }

    private func commit() {
        guard let action = model.chosen else { return }
        run(action)
    }
}

/// `?`: what the keyboard can do from here.
///
/// It carries the vim-mode switch as well as the list. The spec makes vim mode
/// a Settings toggle, and it will end up there too — but the sheet that answers
/// "what are the keys" is where somebody asking that question already is, and
/// `docs/10-cross-cutting/accessibility.md` requires the setting to be
/// reachable rather than merely to exist.
struct CheatSheetView: View {
    @Bindable var preferences: KeyboardPreferences
    let hasList: Bool
    let dismiss: () -> Void

    private var context: KeyboardContext {
        KeyboardContext(hasList: hasList, vimMode: preferences.vimMode)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("Keyboard").font(.title3.weight(.semibold))
                Spacer()
                Button("Done", action: dismiss)
                    .keyboardShortcut(.cancelAction)
            }
            .padding(16)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    ForEach(CheatSheet.sections(for: context)) { section in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(section.title)
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.secondary)
                            ForEach(section.rows) { row in
                                HStack(alignment: .firstTextBaseline) {
                                    Text(row.title)
                                    Spacer(minLength: 20)
                                    Text(row.keys)
                                        .font(.callout.monospaced())
                                        .foregroundStyle(.secondary)
                                }
                                .accessibilityElement(children: .combine)
                            }
                        }
                    }
                }
                .padding(16)
            }
            Divider()
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Vim-style motions", isOn: $preferences.vimMode)
                Text("h j k l, gg, G, u, ⌃R, / and : — on this Mac only, never synced.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding(16)
        }
        .frame(width: 460, height: 560)
    }
}
