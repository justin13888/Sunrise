import SwiftUI

/// Search, narrowing as it is typed: the field with its autocomplete, the
/// Tasks group as ordinary rows, then every other kind the query found, in
/// the core's fixed order, highlighted (`docs/08-features/search.md`
/// §Clients).
///
/// The task rows are `TaskRows`, the same view Today and Inbox use, so a task
/// found here completes, defers and edits identically to one found anywhere
/// else; their titles carry the core's highlight ranges through
/// ``EnvironmentValues/searchHighlights``.
struct SearchView: View {
    @Bindable var model: SearchModel
    let savedViews: SavedViewsModel
    let selection: ListSelection
    let sheets: RowSheets
    let preferences: KeyboardPreferences
    var escapes = ListEscapes()
    var focus: FocusState<PaneFocus?>.Binding
    /// Activate a hit from a group other than Tasks.
    var open: (SearchOpening) -> Void = { _ in }

    @State private var saving = false
    @State private var saveName = ""

    var body: some View {
        VStack(spacing: 0) {
            field
            suggestions
            resultCount

            if model.showsTaskRows {
                TaskRows(
                    model: model.results,
                    selection: selection,
                    sheets: sheets,
                    preferences: preferences,
                    escapes: escapes,
                    focus: focus
                )
                .environment(\.searchHighlights, model.taskHighlights)
            }

            otherGroups
        }
        .navigationTitle(L10n.Search.title)
        .task { focus.wrappedValue = .search }
        .task { await model.follow() }
        // A saved search recalled from the Views menu lands here with its
        // query; the menu can name the screen but not what it searched for.
        .onChange(of: savedViews.pendingSearch, initial: true) { _, query in
            guard let query else { return }
            model.text = query
            savedViews.searchTaken()
        }
        .sheet(isPresented: $saving) {
            SaveViewSheet(name: $saveName, summary: L10n.Search.saveSummary(query: model.canonical)) {
                await savedViews.saveSearch(name: saveName, query: model.canonical)
                saveName = ""
            }
        }
    }

    private var field: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
            TextField(
                L10n.Search.field,
                text: $model.text,
                prompt: Text(L10n.Search.prompt)
            )
            .textFieldStyle(.plain)
            // Search takes the same punctuation capture does — a saved
            // view's summary is `search · /passport · @errands` — so it
            // takes the same input mode.
            .textInput(.syntax)
            .focused(focus, equals: .search)
            .accessibilityIdentifier("search.field")
            .modifier(SearchFieldKeys(model: model, focus: focus))
            if !model.text.isEmpty {
                Button(L10n.Search.clear, systemImage: "xmark.circle.fill") { model.clear() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
            }
            if model.isSearching {
                ProgressView().controlSize(.small)
            }
            Button(L10n.Search.save, systemImage: "bookmark") { saving = true }
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .keyboardShortcut(Keymap.saveSearch)
                .help(Keymap.help(L10n.Search.save, chord: Keymap.saveSearch))
                .disabled(!model.canSave)
                .accessibilityIdentifier("search.save")
        }
        .padding(10)
        .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 8))
        .padding(.horizontal, 12)
        .padding(.top, 10)
    }

    /// The autocomplete, while the field has the keyboard: each operator,
    /// value or name, and what it matches. Tab takes the first; a click takes
    /// any.
    @ViewBuilder
    private var suggestions: some View {
        let offered = focus.wrappedValue == .search ? model.completions : []
        if !offered.isEmpty {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(Array(offered.enumerated()), id: \.offset) { index, completion in
                    Button {
                        model.accept(completion)
                    } label: {
                        HStack(spacing: 10) {
                            Text(completion.label)
                                .font(.callout.monospaced())
                            Text(SearchModel.describe(completion))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                            Spacer(minLength: 0)
                            if index == 0 {
                                Text(Keymap.acceptSuggestion.display)
                                    .font(.caption.monospaced())
                                    .foregroundStyle(.tertiary)
                            }
                        }
                        .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .padding(.vertical, 3)
                    .padding(.horizontal, 8)
                }
            }
            .padding(.vertical, 4)
            .background(.quaternary.opacity(0.25), in: .rect(cornerRadius: 6))
            .padding(.horizontal, 12)
            .padding(.top, 4)
            .accessibilityElement(children: .contain)
            .accessibilityLabel(L10n.Search.suggestions)
            .accessibilityIdentifier("search.suggestions")
        }
    }

    /// How many matches, and for what.
    ///
    /// Named against `searchedText` rather than the field, so the count never
    /// claims to be for a word the query has not run for yet.
    @ViewBuilder
    private var resultCount: some View {
        if !model.searchedText.trimmed.isEmpty, !model.results.tasks.isEmpty {
            HStack {
                Text(matchSummary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .padding(.horizontal, 14)
            .padding(.top, 6)
        }
    }

    private var matchSummary: String {
        let count = model.results.tasks.count
        let capped = count == Int(TaskListKind.searchLimit)
        return capped
            ? L10n.Search.matchesCapped(count: count)
            : L10n.Search.matches(count: count)
    }

    /// Every group after Tasks, each under its heading, in the core's order.
    @ViewBuilder
    private var otherGroups: some View {
        let groups = model.otherGroups
        if !groups.isEmpty {
            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(groups, id: \.kind) { group in
                        SearchGroupSection(group: group, open: open) { model.showAll(group.kind) }
                    }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
            }
            // Beneath task rows the groups are a band; with no task rows
            // they are the whole result.
            .frame(maxHeight: model.showsTaskRows ? 280 : .infinity)
            .accessibilityIdentifier("search.groups")
        }
    }
}

/// The keys the search field answers before the text in it does: Down and
/// Return into the results, Tab for the first suggestion, Backspace on a bare
/// scope token.
private struct SearchFieldKeys: ViewModifier {
    let model: SearchModel
    var focus: FocusState<PaneFocus?>.Binding

    func body(content: Content) -> some View {
        content
            // Down out of the field and into the results, which is where
            // the row keys live. Without it, search is the one list you
            // cannot reach from the keyboard.
            .onKeyPress(.downArrow) {
                focus.wrappedValue = .rows
                return .handled
            }
            .onKeyPress(.return) {
                focus.wrappedValue = .rows
                return .handled
            }
            .onKeyPress(Keymap.acceptSuggestion.keyEquivalent) {
                guard let first = model.completions.first else { return .ignored }
                model.accept(first)
                return .handled
            }
            .onKeyPress(Keymap.removeScope.keyEquivalent) {
                model.deleteBackward() ? .handled : .ignored
            }
    }
}

/// One group of hits other than tasks: its heading, each hit's title and
/// snippet with the matched ranges marked, and "Show all" when it lists
/// fewer than it found.
private struct SearchGroupSection: View {
    let group: SearchGroupItem
    let open: (SearchOpening) -> Void
    let showAll: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(SearchModel.title(of: group))
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .accessibilityAddTraits(.isHeader)
            ForEach(group.hits, id: \.entity) { hit in
                if let opening = SearchModel.opening(hit, in: group.kind) {
                    Button {
                        open(opening)
                    } label: {
                        label(hit)
                    }
                    .buttonStyle(.plain)
                } else {
                    label(hit)
                }
            }
            if SearchModel.isTruncated(group) {
                Button(L10n.Search.showAll(count: Int(group.total)), action: showAll)
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
        }
    }

    private func label(_ hit: SearchHitItem) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(hit.title.attributed)
            if let snippet = hit.snippet {
                Text(snippet.attributed)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(.rect)
    }
}
