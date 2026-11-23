import Foundation

/// What activating a search hit does.
enum SearchOpening: Equatable {
    /// Go to a screen: a stream's or a context's list.
    case destination(Destination)
    /// Show an entity where it lives: a task in its editor, a block on its
    /// day in the calendar, an attachment's task.
    case reveal(EntityRef)

    /// Hand the opening to whichever of a shell's two navigations answers
    /// it: `show` moves to a screen, `reveal` shows an entity where it lives.
    func route(show: (Destination) -> Void, reveal: (EntityRef) -> Void) {
        switch self {
        case let .destination(destination): show(destination)
        case let .reveal(entity): reveal(entity)
        }
    }
}

/// The search field, narrowing as it is typed.
///
/// Owns the text and the debounce. The Tasks group is an ordinary
/// `TaskListModel` on a `.search` kind, so a task found by searching
/// completes, defers, edits and deletes through exactly the same path as one
/// found in Today; a separate results type would have been a second place
/// for those five actions to drift. Every other kind arrives in ``report``,
/// the core's grouped answer to the same query, with its highlight ranges
/// (`docs/08-features/search.md` §Results). The parsing, the grouping, the
/// ranking, the highlights, the autocomplete and the scope token are the
/// core's; this model holds them and never re-tokenizes.
@MainActor
@Observable
final class SearchModel {
    var text: String = "" {
        didSet { textChanged() }
    }

    /// Whether a query is in flight. Distinct from "no results": a field the
    /// user has just typed into has neither yet.
    private(set) var isSearching = false

    let results: TaskListModel

    /// The core's grouped answer for ``searchedText``: every kind with a
    /// hit, in the fixed order, highlighted. `nil` for an empty field.
    private(set) var report: SearchReport?

    /// The token the field was prefilled with when search opened from a
    /// stream's or a context's list, while the field still starts with it.
    /// One Backspace on the bare token removes it whole.
    private(set) var scopeToken: String?

    /// The kind a group's "Show all" re-ran the search for, while the field
    /// still holds its `kind:` operator. Lifts the per-group limit.
    private(set) var expandedKind: SearchKindItem?

    /// How many hits "Show all" lists.
    static let expandedLimit: UInt32 = 500

    private let bridge: CoreBridge
    private let debounce: Duration
    private var pending: _Concurrency.Task<Void, Never>?
    /// The text ``report`` answers.
    private var reportText = ""

    /// 150 ms. Long enough that "passport" is one query rather than eight,
    /// short enough that the list feels attached to the keyboard.
    init(bridge: CoreBridge, debounce: Duration = .milliseconds(150)) {
        self.bridge = bridge
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

    /// The report, when it answers what the rows answer.
    private var currentReport: SearchReport? {
        reportText == searchedText ? report : nil
    }

    /// The groups other than Tasks, whose rows ``results`` draws.
    var otherGroups: [SearchGroupItem] {
        currentReport?.groups.filter { $0.kind != .task } ?? []
    }

    /// The Tasks group's highlighted titles, by task.
    var taskHighlights: [EntityRef: HighlightedText] {
        let tasks = currentReport?.groups.first { $0.kind == .task }?.hits ?? []
        return Dictionary(tasks.map { ($0.entity, $0.title) }) { first, _ in first }
    }

    /// What ⌘S saves: the query on screen in its canonical text, so a saved
    /// search reads the same on every device and in `sunrise-cli`.
    var canonical: String {
        currentReport?.canonical ?? searchedText.trimmed
    }

    var canSave: Bool { !canonical.isEmpty }

    /// The ways to finish the word being typed, best first.
    var completions: [SearchCompletionItem] {
        searchCompletions(
            text: text,
            streams: Array(results.names.streams.values),
            contexts: Array(results.names.contexts.values)
        )
    }

    func accept(_ completion: SearchCompletionItem) {
        text = completion.text
    }

    /// Opened from `destination` (⌘F). A stream's or a context's list
    /// prefills the field with its scope as an editable token; anywhere else
    /// the current query is kept (`docs/08-features/search.md` §Scope).
    func open(from destination: Destination?) {
        guard let token = Self.scopeToken(for: destination), !text.hasPrefix(token) else { return }
        text = token
        scopeToken = token
    }

    /// Backspace in the field. Removes the scope token whole when it is all
    /// the field holds, and says whether it did.
    func deleteBackward() -> Bool {
        guard let token = scopeToken, text == token else { return false }
        text = ""
        return true
    }

    /// Re-run the search for `kind` alone, past the per-group limit.
    func showAll(_ kind: SearchKindItem) {
        expandedKind = kind
        text = "\(text.trimmed) kind:\(searchKindKeyword(kind: kind))"
    }

    func clear() {
        pending?.cancel()
        scopeToken = nil
        expandedKind = nil
        text = ""
        report = nil
        reportText = ""
        Task { await results.show(.search(text: "")) }
    }

    /// Re-run whatever is in the field. ``follow()`` calls this on every
    /// change batch; this is the path for a field that has not changed but
    /// whose answer might have.
    func refresh() async {
        await results.refresh()
        await fetchReport(searchedText)
    }

    /// Follow the change stream for as long as the view is on screen: the
    /// rows and the other groups are re-asked together, so they never answer
    /// different vaults.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    private func textChanged() {
        if let token = scopeToken, !text.hasPrefix(token) { scopeToken = nil }
        if let kind = expandedKind, !text.contains("kind:\(searchKindKeyword(kind: kind))") {
            expandedKind = nil
        }
        scheduleQuery()
    }

    private func scheduleQuery() {
        pending?.cancel()
        let typed = text
        isSearching = !typed.trimmed.isEmpty
        pending = _Concurrency.Task { [debounce, results] in
            try? await _Concurrency.Task.sleep(for: debounce)
            guard !_Concurrency.Task.isCancelled else { return }
            await results.show(.search(text: typed))
            await fetchReport(typed)
            // A late answer for a line the user has moved on from must not
            // claim the field has settled.
            if typed == text { isSearching = false }
        }
    }

    /// Ask the core for every group. `prefix` is off so the groups answer
    /// exactly the query the Tasks rows answer (`Query::Search` adds no
    /// prefix); the trigram index already finds a word's start.
    private func fetchReport(_ typed: String) async {
        let trimmed = typed.trimmed
        guard !trimmed.isEmpty else {
            report = nil
            reportText = typed
            return
        }
        let now = await bridge.nowMs()
        let query = CoreQuery.searchGrouped(
            text: trimmed,
            nowMs: now,
            prefix: false,
            perGroup: expandedKind == nil ? nil : Self.expandedLimit
        )
        guard case let .search(found)? = try? await bridge.query(query),
              searchedText == typed else { return }
        report = found
        reportText = typed
    }
}

// MARK: - What the view asks of a hit, a group and a suggestion

extension SearchModel {
    /// The token a list prefills the field with: `#work `, `@errands `.
    static func scopeToken(for destination: Destination?) -> String? {
        guard case let .list(kind)? = destination else { return nil }
        switch kind {
        case let .stream(_, name): return searchScopeToken(scope: .stream, name: name)
        case let .context(_, name): return searchScopeToken(scope: .context, name: name)
        default: return nil
        }
    }

    /// What activating `hit` in a group of `kind` opens. A stream or a
    /// context is a list of its own; everything else is shown where it
    /// lives, through the same reveal a `sunrise://entity/` link takes.
    static func opening(_ hit: SearchHitItem, in kind: SearchKindItem) -> SearchOpening {
        switch kind {
        case .stream: .destination(.list(.stream(id: hit.opens, name: hit.title.text)))
        case .context: .destination(.list(.context(id: hit.opens, name: hit.title.text)))
        default: .reveal(hit.opens)
        }
    }

    /// A group's heading: "Streams (3)".
    static func title(of group: SearchGroupItem) -> String {
        let titles: [SearchKindItem: (Int) -> String] = [
            .task: L10n.Search.groupTask, .note: L10n.Search.groupNote,
            .event: L10n.Search.groupEvent, .block: L10n.Search.groupBlock,
            .stream: L10n.Search.groupStream, .context: L10n.Search.groupContext,
            .place: L10n.Search.groupPlace, .attachment: L10n.Search.groupAttachment,
            .view: L10n.Search.groupView, .person: L10n.Search.groupPerson
        ]
        let count = Int(group.total)
        return titles[group.kind]?(count) ?? L10n.Search.matches(count: count)
    }

    /// Whether a group holds more hits than it lists.
    static func isTruncated(_ group: SearchGroupItem) -> Bool {
        Int(group.total) > group.hits.count
    }

    /// What a suggestion matches, in the reader's language. The operator
    /// itself is shown beside it and is ASCII English on every locale.
    static func describe(_ completion: SearchCompletionItem) -> String {
        switch completion.kind {
        case .stream: return L10n.Search.nameStream
        case .context: return L10n.Search.nameContext
        case .key: return operatorDescription(completion.key)
        case .value:
            let values = completion.key == "is" ? isDescriptions
                : completion.key == "has" ? hasDescriptions : [:]
            return completion.value.flatMap { values[$0] } ?? operatorDescription(completion.key)
        }
    }

    private static func operatorDescription(_ key: String) -> String {
        let descriptions: [String: String] = [
            "stream": L10n.Search.opStream, "context": L10n.Search.opContext,
            "state": L10n.Search.opState, "kind": L10n.Search.opKind,
            "is": L10n.Search.opIs, "has": L10n.Search.opHas,
            "priority": L10n.Search.opPriority, "planned": L10n.Search.opPlanned,
            "target": L10n.Search.opTarget, "hard": L10n.Search.opHard,
            "due": L10n.Search.opDue, "created": L10n.Search.opCreated,
            "done": L10n.Search.opDone, "place": L10n.Search.opPlace,
            "date": L10n.Search.opDate
        ]
        // A newer core may name an operator this build has no words for.
        return descriptions[key] ?? ""
    }

    private static var isDescriptions: [String: String] {
        [
            "late": L10n.Search.isLate, "late-hard": L10n.Search.isLateHard,
            "stale": L10n.Search.isStale, "blocked": L10n.Search.isBlocked,
            "recurring": L10n.Search.isRecurring, "unplanned": L10n.Search.isUnplanned,
            "inbox": L10n.Search.isInbox, "fixed": L10n.Search.isFixed,
            "flexible": L10n.Search.isFlexible
        ]
    }

    private static var hasDescriptions: [String: String] {
        [
            "attachment": L10n.Search.hasAttachment, "note": L10n.Search.hasNote,
            "estimate": L10n.Search.hasEstimate, "deadline": L10n.Search.hasDeadline,
            "place": L10n.Search.hasPlace, "dependency": L10n.Search.hasDependency
        ]
    }
}

// MARK: - Highlights

/// One run of a highlighted text: a stretch that matched, or one that did
/// not.
struct HighlightRun: Equatable {
    let text: String
    let matched: Bool
}

extension HighlightedText {
    /// The text cut at its ranges, in order. The ranges are Unicode-scalar
    /// offsets, which is how `String.unicodeScalars` counts, so nothing is
    /// re-tokenized here. A range out of order, overlapping one before it,
    /// or past the end is clamped or skipped rather than trusted.
    var runs: [HighlightRun] {
        let scalars = Array(text.unicodeScalars)
        func piece(_ from: Int, _ to: Int) -> String {
            var view = String.UnicodeScalarView()
            view.append(contentsOf: scalars[from..<to])
            return String(view)
        }
        var out: [HighlightRun] = []
        var at = 0
        for range in ranges {
            let start = min(Int(range.start), scalars.count)
            let end = min(Int(range.end), scalars.count)
            guard start >= at, end > start else { continue }
            if start > at { out.append(HighlightRun(text: piece(at, start), matched: false)) }
            out.append(HighlightRun(text: piece(start, end), matched: true))
            at = end
        }
        if at < scalars.count { out.append(HighlightRun(text: piece(at, scalars.count), matched: false)) }
        return out
    }
}
