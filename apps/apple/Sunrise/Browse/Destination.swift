import Foundation

/// Which list a task view is showing.
///
/// The stream and context cases carry their name as well as their id. A
/// sidebar selection outlives one refresh of the sidebar, and a title that
/// went blank between the click and the next `StreamList` read would be a
/// flicker with no cause the user can see.
enum TaskListKind: Equatable, Hashable, Identifiable {
    /// Today, optionally narrowed to a set of contexts.
    ///
    /// `Query::Today` takes the filter itself, so a narrowed Today is one
    /// query rather than a list this client sieves afterwards — and a saved
    /// view that names contexts recalls a *filtered* Today rather than
    /// silently dropping the part of itself it could not express.
    case today(contexts: [EntityRef])
    case inbox
    case stream(id: EntityRef, name: String)
    case context(id: EntityRef, name: String)
    /// Full-text search. The text is part of the kind so that a re-query on a
    /// change batch searches for what is in the field *now* rather than for
    /// whatever the last keystroke happened to be.
    case search(text: String)

    var id: Self { self }

    /// Today with no filter — the sidebar's own entry.
    static let todayAll = TaskListKind.today(contexts: [])

    /// Whether this is Today, filtered or not. Today is the only list the
    /// core sections by urgency, so it is the only one that groups.
    var isToday: Bool {
        if case .today = self { return true }
        return false
    }

    var title: String {
        switch self {
        case let .today(contexts):
            contexts.isEmpty ? L10n.Browse.today : L10n.Browse.todayFiltered(count: contexts.count)
        case .inbox: L10n.Browse.inbox
        case let .stream(_, name): name
        case let .context(_, name): "@\(name)"
        case .search: L10n.Browse.search
        }
    }

    var symbol: String {
        switch self {
        case .today: "sun.max"
        case .inbox: "tray"
        case .stream: "number"
        case .context: "at"
        case .search: "magnifyingglass"
        }
    }

    var emptyMessage: String {
        switch self {
        case let .today(contexts):
            contexts.isEmpty
                ? L10n.Browse.emptyToday
                : L10n.Browse.emptyTodayFiltered
        case .inbox: L10n.Browse.emptyInbox
        case let .stream(_, name): L10n.Browse.emptyStream(name: name)
        case let .context(_, name): L10n.Browse.emptyContext(name: name)
        case let .search(text):
            text.trimmed.isEmpty
                ? L10n.Browse.emptySearchBlank
                : L10n.Browse.emptySearch(text: text.trimmed)
        }
    }

    /// The key for the one action that fills this view — `docs/08-features/keyboard.md`
    /// Rule 1's empty-state row. Capture where a capture would land here;
    /// on Search, the field, and then a fresh query. Empty for a list nothing
    /// typed can fill: a context, or a filtered Today, refuses capture.
    var emptyHint: String {
        switch self {
        case .today, .inbox, .stream:
            acceptsCapture ? Keymap.pressHint(.quickCapture, L10n.Browse.hintCapture(keys:)) : ""
        case .context:
            ""
        case let .search(text) where text.trimmed.isEmpty:
            Keymap.pressHint(.searchInView, L10n.Browse.hintSearchField(keys:))
        case .search:
            Keymap.pressHint(.searchGlobal, L10n.Browse.hintSearchNew(keys:))
        }
    }

    /// What the empty view says: ``emptyMessage``, then ``emptyHint`` where
    /// the device has a keyboard to press it on — a phone's empty Inbox
    /// telling its owner to press ⌘N would be teaching a key they do not have.
    func emptyDescription(hinted: Bool) -> String {
        let hint = hinted ? emptyHint : ""
        return hint.isEmpty ? emptyMessage : "\(emptyMessage) \(hint)"
    }

    /// The read behind this list.
    ///
    /// `nowMs` is only consulted by Today, which is the only list whose
    /// contents depend on the clock.
    func query(nowMs: UInt64) -> CoreQuery {
        switch self {
        case let .today(contexts): .today(nowMs: nowMs, contexts: contexts)
        case .inbox: .inbox
        case let .stream(id, _): .streamTasks(stream: id)
        case let .context(id, _): .contextTasks(context: id)
        case let .search(text): .search(text: text.trimmed, limit: TaskListKind.searchLimit)
        }
    }

    /// How many rows a search returns.
    ///
    /// A cap, not a page: search narrows as you type, and someone looking at
    /// 200 matches is going to type another word rather than scroll.
    static let searchLimit: UInt32 = 200

    /// Whether a new capture lands somewhere this list would show it.
    ///
    /// A context list is where it does not: capture writes to a stream, and
    /// there is no way to say "give this the context I am looking at" without
    /// inventing an annotation the shared parser does not have.
    ///
    /// A *filtered* Today is the same list under a different name — the filter
    /// is a set of contexts, and a captured line carries none of them, so the
    /// row would be written and then filtered straight back out. Unfiltered
    /// Today does accept capture, because ``TaskListModel/create(_:)`` gives
    /// the line the date that list selects on.
    var acceptsCapture: Bool {
        switch self {
        case let .today(contexts): contexts.isEmpty
        case .inbox, .stream: true
        case .context, .search: false
        }
    }

    /// Whether running this list's query is worth a round trip.
    ///
    /// An empty search is not: `Query::Search` over an empty string is a
    /// full-table scan whose answer nobody asked for.
    var isWorthQuerying: Bool {
        if case let .search(text) = self { return !text.trimmed.isEmpty }
        return true
    }
}

/// Everything the sidebar can select.
///
/// One type rather than a selection per section: `NavigationSplitView` takes a
/// single selection binding, and two of them would let the app be on two
/// screens at once.
enum Destination: Equatable, Hashable, Identifiable {
    case list(TaskListKind)
    case search
    case calendar
    case focus
    case routines
    case review
    /// The morning summary, backed by `Query::MorningSummary`.
    case morning
    /// The end-of-day plan, backed by `Query::EndOfDayPlan`.
    case evening

    var id: Self { self }

    /// The primary views every client presents, in the order
    /// `docs/07-clients/parity-matrix.md` lists them. Streams and contexts are
    /// not here: they are data, and the sidebar reads them from the vault.
    ///
    /// The two briefs bracket the list because that is when they are read.
    /// They are also the only entries a *notification* can open — see
    /// ``DeepLink`` — and putting them in the sidebar rather than behind an
    /// alert means someone who never allows notifications still has them.
    static let fixed: [Destination] = [
        .morning, .list(.todayAll), .list(.inbox), .search,
        .calendar, .focus, .routines, .review, .evening
    ]

    var title: String {
        switch self {
        case let .list(kind): kind.title
        case .search: L10n.Search.title
        case .calendar: L10n.Browse.calendar
        case .focus: L10n.Focus.title
        case .routines: L10n.Browse.routines
        case .review: L10n.Browse.review
        case .morning: L10n.Brief.morningTitle
        case .evening: L10n.Brief.eveningTitle
        }
    }

    var symbol: String {
        switch self {
        case let .list(kind): kind.symbol
        case .search: "magnifyingglass"
        case .calendar: "calendar"
        case .focus: "timer"
        case .routines: "repeat"
        case .review: "chart.line.uptrend.xyaxis"
        case .morning: "sunrise"
        case .evening: "moon.stars"
        }
    }
}

extension Destination {
    /// The context names a saved view of this screen should carry.
    ///
    /// Only a filtered Today or a context list has any; everything else saves
    /// no filter rather than one it could not honour on recall. Names rather
    /// than ids, because `SavedViewsModel` stores names — an `EntityRef` is a
    /// vault-local ULID and would resolve to nothing on a paired Mac.
    func contextNames(in names: NameBook) -> [String] {
        guard case let .list(kind) = self else { return [] }
        switch kind {
        case let .today(contexts): return contexts.compactMap { names.contexts[$0] }
        case let .context(_, name): return [name]
        case .inbox, .stream, .search: return []
        }
    }
}
