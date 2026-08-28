import Foundation

/// Which list a task view is showing.
///
/// The stream and context cases carry their name as well as their id. A
/// sidebar selection outlives one refresh of the sidebar, and a title that
/// went blank between the click and the next `StreamList` read would be a
/// flicker with no cause the user can see.
enum TaskListKind: Equatable, Hashable, Identifiable {
    case today
    case inbox
    case stream(id: EntityRef, name: String)
    case context(id: EntityRef, name: String)

    var id: Self { self }

    var title: String {
        switch self {
        case .today: "Today"
        case .inbox: "Inbox"
        case let .stream(_, name): name
        case let .context(_, name): "@\(name)"
        }
    }

    var symbol: String {
        switch self {
        case .today: "sun.max"
        case .inbox: "tray"
        case .stream: "number"
        case .context: "at"
        }
    }

    var emptyMessage: String {
        switch self {
        case .today: "Nothing is scheduled or due today."
        case .inbox: "Your Inbox is empty."
        case let .stream(_, name): "Nothing in \(name) yet."
        case let .context(_, name): "Nothing carries @\(name)."
        }
    }

    /// The read behind this list.
    ///
    /// `nowMs` is only consulted by Today, which is the only list whose
    /// contents depend on the clock.
    func query(nowMs: UInt64) -> CoreQuery {
        switch self {
        case .today: .today(nowMs: nowMs, contexts: [])
        case .inbox: .inbox
        case let .stream(id, _): .streamTasks(stream: id)
        case let .context(id, _): .contextTasks(context: id)
        }
    }

    /// Whether a new capture lands somewhere this list would show it.
    ///
    /// A context list is the one place it does not: capture writes to a
    /// stream, and there is no way to say "give this the context I am looking
    /// at" without inventing an annotation the shared parser does not have.
    var acceptsCapture: Bool {
        if case .context = self { return false }
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
    case focus
    case routines
    case review

    var id: Self { self }

    /// The primary views every client presents, in the order
    /// `docs/07-clients/parity-matrix.md` lists them. Streams and contexts are
    /// not here: they are data, and the sidebar reads them from the vault.
    static let fixed: [Destination] = [
        .list(.today), .list(.inbox), .search, .focus, .routines, .review
    ]

    var title: String {
        switch self {
        case let .list(kind): kind.title
        case .search: "Search"
        case .focus: "Focus"
        case .routines: "Routines"
        case .review: "Review"
        }
    }

    var symbol: String {
        switch self {
        case let .list(kind): kind.symbol
        case .search: "magnifyingglass"
        case .focus: "timer"
        case .routines: "repeat"
        case .review: "chart.line.uptrend.xyaxis"
        }
    }
}
