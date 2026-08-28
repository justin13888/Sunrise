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
        case let .today(contexts): contexts.isEmpty ? "Today" : "Today · \(contexts.count) contexts"
        case .inbox: "Inbox"
        case let .stream(_, name): name
        case let .context(_, name): "@\(name)"
        case .search: "Search"
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
                ? "Nothing is scheduled or due today."
                : "Nothing in those contexts is scheduled or due today."
        case .inbox: "Your Inbox is empty."
        case let .stream(_, name): "Nothing in \(name) yet."
        case let .context(_, name): "Nothing carries @\(name)."
        case let .search(text):
            text.trimmed.isEmpty
                ? "Type to search titles and notes."
                : "Nothing matches \u{201c}\(text.trimmed)\u{201d}."
        }
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
    /// A context list is the one place it does not: capture writes to a
    /// stream, and there is no way to say "give this the context I am looking
    /// at" without inventing an annotation the shared parser does not have.
    var acceptsCapture: Bool {
        switch self {
        case .today, .inbox, .stream: true
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
        case .search: "Search"
        case .calendar: "Calendar"
        case .focus: "Focus"
        case .routines: "Routines"
        case .review: "Review"
        case .morning: "Morning"
        case .evening: "Evening"
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
