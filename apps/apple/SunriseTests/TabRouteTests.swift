#if os(iOS)
import Foundation
import Testing

@testable import Sunrise

/// Where a `Destination` lands in the iOS tab shell.
///
/// iOS-only because the type is: on macOS a destination is a sidebar
/// selection and there is nothing to decide. Here the same request has to
/// choose a tab *and* a stack, and every route into the app — a `sunrise://`
/// link, a tapped reminder, an App Intent, a widget — goes through it.
struct TabRouteTests {
    /// Today is the app's front door, so it is a tab root rather than
    /// something pushed onto one.
    @Test
    func todayIsATabRootWithNothingPushed() {
        let route = TabRoute.route(to: .list(.todayAll))
        #expect(route.tab == .today)
        #expect(route.path.isEmpty)
    }

    /// A *filtered* Today is still Today, and it is pushed rather than made
    /// the root — the unfiltered list has to stay reachable behind it, or a
    /// saved view becomes a one-way trip.
    @Test
    func aFilteredTodayIsPushedOnTopOfTheUnfilteredOne() {
        let filtered = TaskListKind.today(contexts: ["ctx_1"])
        let route = TabRoute.route(to: .list(filtered))
        #expect(route.tab == .today)
        #expect(route.path == [.list(filtered)])
    }

    /// The Inbox, a stream and a context are all browsing, so they push onto
    /// Browse rather than replacing a tab. Landing on a stream with no way
    /// back to the stream list is the dead end a tab bar makes easy.
    @Test
    func everyBrowsableListPushesOntoBrowse() {
        let kinds: [TaskListKind] = [
            .inbox,
            .stream(id: "str_1", name: "Errands"),
            .context(id: "ctx_1", name: "home")
        ]
        for kind in kinds {
            let route = TabRoute.route(to: .list(kind))
            #expect(route.tab == .browse, "\(kind) should browse")
            #expect(route.path == [.list(kind)])
        }
    }

    /// The four screens a phone's tab bar cannot hold are reachable, and they
    /// are reachable by being pushed onto Browse rather than by being dropped.
    @Test
    func theScreensWithoutATabArePushedOntoBrowse() {
        for destination: Destination in [.routines, .review, .morning, .evening] {
            let route = TabRoute.route(to: destination)
            #expect(route.tab == .browse, "\(destination) should push onto Browse")
            #expect(route.path == [destination])
        }
    }

    /// **The bar holds four items, not five.** `Tab(role: .search)` is placed
    /// by the system but still counts against the bar's capacity, and a fifth
    /// content tab beside it tips the whole `TabView` into a system-generated
    /// "More" list — which silently re-homes the app's own tabs one level
    /// deeper. That was observed in the accessibility tree, not theorised, and
    /// this is the assertion that keeps a sixth tab from being added back.
    @Test
    func theBarStaysWithinTheCapacityThatAvoidsASystemMoreList() {
        #expect(AppTab.bar.count == 4)
        #expect(AppTab.allCases.count == 5, "four tabs plus search")
    }

    /// Search has a tab of its own, and both ways of asking for it agree.
    @Test
    func bothSearchDestinationsReachTheSearchTab() {
        #expect(TabRoute.route(to: .search).tab == .search)
        #expect(TabRoute.route(to: .list(.search(text: "passport"))).tab == .search)
    }

    /// The tabs with nothing to push resolve to a bare root.
    @Test
    func calendarAndFocusAreTabRoots() {
        #expect(TabRoute.route(to: .calendar) == TabRoute(tab: .calendar))
        #expect(TabRoute.route(to: .focus) == TabRoute(tab: .focus))
    }

    /// **Every destination the sidebar offers has somewhere to go.** The macOS
    /// sidebar lists nine, the tab bar holds five, and the mapping between
    /// them is hand-written — so the failure this guards against is a tenth
    /// destination being added and silently having no iOS home.
    @Test
    func everyFixedDestinationRoutesSomewhere() {
        for destination in Destination.fixed {
            let route = TabRoute.route(to: destination)
            #expect(
                AppTab.allCases.contains(route.tab),
                "\(destination) routed to an unknown tab"
            )
            #expect(route.path.count <= 1, "\(destination) pushed more than one screen")
        }
    }

    /// A tab that the bar does not draw cannot be reached by tapping, so
    /// anything routed to one has to be reachable another way. Search is the
    /// only such tab, and the system places it.
    @Test
    func theOnlyTabOutsideTheBarIsSearch() {
        let outside = Set(AppTab.allCases).subtracting(AppTab.bar)
        #expect(outside == [.search])
    }
}
#endif
