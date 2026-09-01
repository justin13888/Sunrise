import Foundation

/// The search field, narrowing as it is typed.
///
/// Owns the text and the debounce; the results are an ordinary `TaskListModel`
/// on a `.search` kind, so a task found by searching completes, defers, edits
/// and deletes through exactly the same path as one found in Today. A separate
/// results type would have been a second place for those five actions to
/// drift.
@MainActor
@Observable
final class SearchModel {
    var text: String = "" {
        didSet { scheduleQuery() }
    }

    /// Whether a query is in flight. Distinct from "no results": a field the
    /// user has just typed into has neither yet.
    private(set) var isSearching = false

    let results: TaskListModel

    private let debounce: Duration
    private var pending: _Concurrency.Task<Void, Never>?

    /// 150 ms. Long enough that "passport" is one query rather than eight,
    /// short enough that the list feels attached to the keyboard.
    init(bridge: CoreBridge, debounce: Duration = .milliseconds(150)) {
        results = TaskListModel(bridge: bridge, kind: .search(text: ""))
        self.debounce = debounce
    }

    /// The text the results on screen actually answer.
    ///
    /// Read off the results model rather than off `text`, so an empty-state
    /// message never names a word the query has not run for yet.
    var searchedText: String {
        if case let .search(text) = results.kind { return text }
        return ""
    }

    func clear() {
        pending?.cancel()
        text = ""
        Task { await results.show(.search(text: "")) }
    }

    /// Re-run whatever is in the field. The change stream calls this through
    /// `results.follow()`; this is the path for a field that has not changed
    /// but whose answer might have.
    func refresh() async {
        await results.refresh()
    }

    private func scheduleQuery() {
        pending?.cancel()
        let typed = text
        isSearching = !typed.trimmed.isEmpty
        pending = _Concurrency.Task { [debounce, results] in
            try? await _Concurrency.Task.sleep(for: debounce)
            guard !_Concurrency.Task.isCancelled else { return }
            await results.show(.search(text: typed))
            // A late answer for a line the user has moved on from must not
            // claim the field has settled.
            if typed == text { isSearching = false }
        }
    }
}
