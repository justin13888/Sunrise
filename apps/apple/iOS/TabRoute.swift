import Foundation

/// The tabs the iOS shell presents.
///
/// **Four, plus search — five in total, and the total is the constraint.**
/// `Tab(role: .search)` is *not* placed outside the count: with five content
/// tabs beside it the system decides the bar has overflowed and generates its
/// own "More" list, which put this app's own More tab one level inside a
/// system More of the same name. Measured, not assumed — the accessibility
/// tree showed a `More` cell and a `Search` cell where the app's rows should
/// have been.
///
/// So the sidebar's nine `Destination.fixed` entries map to four tabs and a
/// menu. A Mac sidebar is a list that can be nine items long; a phone's tab
/// bar is not, and the honest way to say so is a menu rather than a sixth tab
/// the system will quietly re-home.
enum AppTab: String, Hashable, CaseIterable, Identifiable, Sendable {
    case today
    case calendar
    case browse
    case focus
    case search

    var id: String { rawValue }

    var title: String {
        switch self {
        case .today: "Today"
        case .calendar: "Calendar"
        case .browse: "Browse"
        case .focus: "Focus"
        case .search: "Search"
        }
    }

    var symbol: String {
        switch self {
        case .today: "sun.max"
        case .calendar: "calendar"
        case .browse: "tray.full"
        case .focus: "timer"
        case .search: "magnifyingglass"
        }
    }

    /// The tabs drawn as ordinary items, in order. ``search`` is excluded
    /// because `Tab(role: .search)` is positioned by the system — but it still
    /// counts toward the bar's capacity, which is why there are four here and
    /// not five.
    static let bar: [AppTab] = [.today, .calendar, .browse, .focus]
}

/// Where a `Destination` lands in the tab shell.
///
/// A pure function with its own tests, and that is the point. On macOS a
/// destination is a sidebar *selection* — one binding, one list, nothing to
/// decide. On iOS the same request has to choose a tab **and** a stack to push
/// onto, and every route into the app asks for one: a `sunrise://` link, a
/// tapped reminder, an App Intent, the command palette, a widget. Deciding it
/// inside a view body would put the one piece of iOS navigation logic that can
/// actually be wrong in the one place a test cannot reach.
struct TabRoute: Equatable, Sendable {
    let tab: AppTab
    /// What to push onto that tab's stack. Empty means the tab's own root.
    ///
    /// Only ever zero or one deep: every destination is either a tab root or
    /// one screen inside a tab. Modelled as an array because that is what
    /// `NavigationStack` binds to.
    let path: [Destination]

    init(tab: AppTab, path: [Destination] = []) {
        self.tab = tab
        self.path = path
    }

    /// Resolve a destination to the tab that shows it.
    ///
    /// The three list kinds split deliberately. Today is a tab of its own
    /// because it is the screen the app opens on; the Inbox, a stream and a
    /// context are all *browsing*, so they push onto Browse, which is where
    /// their parent list already is — landing on a stream with no way back to
    /// the stream list is the navigation dead end a tab bar makes easy.
    static func route(to destination: Destination) -> TabRoute {
        switch destination {
        case let .list(kind):
            switch kind {
            case .today:
                // A *filtered* Today still belongs on the Today tab, and it is
                // pushed rather than made the root: the filter came from a
                // saved view or a link, and the unfiltered list has to stay
                // reachable behind it.
                if kind == .todayAll {
                    TabRoute(tab: .today)
                } else {
                    TabRoute(tab: .today, path: [destination])
                }
            case .inbox, .stream, .context:
                TabRoute(tab: .browse, path: [destination])
            case .search:
                TabRoute(tab: .search)
            }
        case .search:
            TabRoute(tab: .search)
        case .calendar:
            TabRoute(tab: .calendar)
        case .focus:
            TabRoute(tab: .focus)
        case .routines, .review, .morning, .evening:
            // Pushed onto Browse rather than given a tab of their own. These
            // four are read once or twice a day rather than lived in, and
            // Browse is already the tab that means "everything else this vault
            // contains" — so its stack is where they belong, and the toolbar
            // menu that opens them sits on the same screen.
            TabRoute(tab: .browse, path: [destination])
        }
    }
}
