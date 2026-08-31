import SwiftUI

/// Full-text search, narrowing as it is typed.
///
/// The rows are `TaskRows`, the same view Today and Inbox use, so a task found
/// here completes, defers and edits identically to one found anywhere else.
struct SearchView: View {
    @Bindable var model: SearchModel
    let selection: ListSelection
    let sheets: RowSheets
    let preferences: KeyboardPreferences
    var escapes = ListEscapes()
    var focus: FocusState<PaneFocus?>.Binding

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(.secondary)
                TextField(
                    "Search",
                    text: $model.text,
                    prompt: Text("Titles and notes")
                )
                .textFieldStyle(.plain)
                .focused(focus, equals: .search)
                .accessibilityIdentifier("search.field")
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
                if !model.text.isEmpty {
                    Button("Clear", systemImage: "xmark.circle.fill") { model.clear() }
                        .labelStyle(.iconOnly)
                        .buttonStyle(.plain)
                        .foregroundStyle(.secondary)
                }
                if model.isSearching {
                    ProgressView().controlSize(.small)
                }
            }
            .padding(10)
            .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 8))
            .padding(.horizontal, 12)
            .padding(.top, 10)

            resultCount

            TaskRows(
                model: model.results,
                selection: selection,
                sheets: sheets,
                preferences: preferences,
                escapes: escapes,
                focus: focus
            )
        }
        .navigationTitle("Search")
        .task { focus.wrappedValue = .search }
        .task { await model.results.follow() }
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
        let noun = count == 1 ? "match" : "matches"
        return capped
            ? "First \(count) \(noun) — narrow it further"
            : "\(count) \(noun)"
    }
}
