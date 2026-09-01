import SwiftUI

/// The unlocked app on iOS and iPadOS.
///
/// The Mac's counterpart is `macOS/VaultWindow.swift`, and the two differ in
/// navigation and nothing else: both build one ``VaultModels`` from one bridge
/// and hand the same screens the same models. What changes is the shape.
///
/// `.sidebarAdaptable` is doing real work here. It draws a tab bar on iPhone
/// and a sidebar on iPad from one declaration — so the iPad gets a layout much
/// closer to the Mac's without a third shell to maintain, which is what
/// `docs/07-clients/shared-ui-system.md` means by each platform owning its
/// navigation idiom while everything below it stays shared.
struct VaultTabs: View {
    let bridge: CoreBridge
    let session: SessionModel
    let surfaces: AppSurfaces

    @State private var models: VaultModels
    @State private var tab: AppTab = .today
    @State private var todayPath: [Destination] = []
    @State private var browsePath: [Destination] = []
    @State private var showingSettings = false
    @State private var deviceID = ""

    /// Shared with the Mac: one row selection and one set of row sheets for
    /// the whole app, so "Schedule…" means the same thing however it was
    /// asked for.
    @State private var keys = KeyboardPreferences()
    @State private var rows = ListSelection()
    @State private var sheets = RowSheets()
    @State private var palette = CommandPaletteModel()
    @State private var showingCheatSheet = false
    @State private var creatingStream = false

    init(bridge: CoreBridge, session: SessionModel, surfaces: AppSurfaces) {
        self.bridge = bridge
        self.session = session
        self.surfaces = surfaces
        _models = State(initialValue: VaultModels(bridge: bridge))
    }

    var body: some View {
        tabs
            .modifier(CaptureSheet(surfaces: surfaces))
            .modifier(rowSheets)
            .sheet(isPresented: $showingSettings) { settingsSheet }
            .modifier(Routing(surfaces: surfaces, show: show, perform: perform))
            .modifier(Lifecycle(
                bridge: bridge,
                models: models,
                surfaces: surfaces,
                deviceID: $deviceID,
                startSync: startSync
            ))
    }

    /// Split out of `body`, and not for tidiness: with the six modifiers above
    /// inline, the type checker gives up on the whole expression.
    private var tabs: some View {
        TabView(selection: $tab) {
            Tab(AppTab.today.title, systemImage: AppTab.today.symbol, value: AppTab.today) {
                NavigationStack(path: $todayPath) { todayRoot }
            }
            // Capture belongs on these two for the same reason it is on Today
            // and Browse, and `openCapture` has always assumed it was there:
            // a screen with no inline bar is precisely where it opens the
            // sheet instead. Without the button, that branch — and the whole
            // sheet with it — was reachable only from a `sunrise://` link.
            Tab(AppTab.calendar.title, systemImage: AppTab.calendar.symbol, value: AppTab.calendar) {
                NavigationStack {
                    CalendarView(model: models.calendar)
                        .navigationTitle("Calendar")
                        .toolbar { captureButton }
                }
            }
            Tab(AppTab.browse.title, systemImage: AppTab.browse.symbol, value: AppTab.browse) {
                NavigationStack(path: $browsePath) { browseRoot }
            }
            Tab(AppTab.focus.title, systemImage: AppTab.focus.symbol, value: AppTab.focus) {
                NavigationStack {
                    FocusView(model: models.focus)
                        .navigationTitle("Focus")
                        .toolbar { captureButton }
                }
            }
            // The search tab counts toward the bar's capacity even though the
            // system positions it, so there are four tabs above and not five.
            // See ``AppTab``.
            Tab(value: AppTab.search, role: .search) {
                NavigationStack { searchRoot }
            }
        }
        .tabViewStyle(.sidebarAdaptable)
    }

    /// The row sheets — edit, schedule, defer, move — reusing the Mac's
    /// modifier so a sheet cannot behave differently on the two platforms.
    ///
    /// The palette and the cheat sheet are handed inert values: both are
    /// keyboard surfaces, and the screens they reach are all in the tab bar or
    /// one tap into More.
    private var rowSheets: KeyboardSurfaces {
        KeyboardSurfaces(
            sheets: sheets,
            palette: palette,
            preferences: keys,
            list: activeList,
            hasList: tab == .today || tab == .browse || tab == .search,
            showingCheatSheet: $showingCheatSheet,
            creatingStream: $creatingStream,
            perform: perform,
            createStream: { name, edit in
                await models.browse.createStream(
                    name: name,
                    color: edit.color,
                    cadence: edit.reviewCadence
                )
            }
        )
    }

    // MARK: - Tab roots

    private var todayRoot: some View {
        TaskListView(
            model: models.list,
            capture: models.capture,
            selection: rows,
            sheets: sheets,
            preferences: keys,
            escapes: escapes,
            focus: $pane
        )
        .navigationTitle("Today")
        .navigationDestination(for: Destination.self) { pushed(destination: $0) }
        .toolbar { captureButton }
        .task { await models.list.show(.todayAll) }
    }

    private var browseRoot: some View {
        BrowseSidebar(model: models.browse, selection: browseSelection)
            .navigationTitle("Browse")
            .navigationDestination(for: Destination.self) { pushed(destination: $0) }
            .toolbar {
                captureButton
                overflowMenu
            }
    }

    private var searchRoot: some View {
        SearchView(
            model: models.search,
            selection: rows,
            sheets: sheets,
            preferences: keys,
            escapes: escapes,
            focus: $pane
        )
        .navigationTitle("Search")
        .toolbar { captureButton }
    }

    /// What a phone's tab bar cannot hold.
    ///
    /// A menu rather than a sixth tab, and that is measured rather than
    /// stylistic: a fifth content tab beside the search tab makes the system
    /// generate its own "More" list and re-home this app's tabs inside it. A
    /// toolbar menu is what iOS offers for secondary destinations, and it
    /// keeps every one of them exactly one tap away.
    @ToolbarContentBuilder
    private var overflowMenu: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Menu("More", systemImage: "ellipsis.circle") {
                Section("Today") {
                    menuLink(to: .morning)
                    menuLink(to: .evening)
                }
                Section {
                    menuLink(to: .routines)
                    menuLink(to: .review)
                }
                Section {
                    Button(models.undo.undoTitle, systemImage: "arrow.uturn.backward") {
                        Task { await models.undo.undo() }
                    }
                    .disabled(!models.undo.canUndo)
                    Button("Settings", systemImage: "gearshape") { showingSettings = true }
                }
            }
            .accessibilityIdentifier("more")
        }
    }

    /// A menu row that pushes onto Browse's stack.
    ///
    /// A `Button` rather than a `NavigationLink`: a link inside a `Menu` is
    /// not a link the surrounding `NavigationStack` will follow, so the push
    /// is performed explicitly — through the same ``show(_:)`` every other
    /// route uses, which is what keeps a menu tap and a deep link landing in
    /// the same place.
    private func menuLink(to destination: Destination) -> some View {
        Button(destination.title, systemImage: destination.symbol) {
            show(destination)
        }
    }

    /// One screen, pushed. The same views the Mac puts in its detail pane —
    /// the detail *pane* is what iOS replaces with a push, per
    /// `shared-ui-system.md`'s component table, not the views inside it.
    @ViewBuilder
    private func pushed(destination: Destination) -> some View {
        switch destination {
        case let .list(kind):
            TaskListView(
                model: models.list,
                capture: models.capture,
                selection: rows,
                sheets: sheets,
                preferences: keys,
                escapes: escapes,
                focus: $pane
            )
            .navigationTitle(kind.title)
            .toolbar { captureButton }
            .task { await models.list.show(kind) }
        case .calendar:
            CalendarView(model: models.calendar).navigationTitle("Calendar")
        case .focus:
            FocusView(model: models.focus).navigationTitle("Focus")
        case .routines:
            RoutinesView(model: models.routines).navigationTitle("Routines")
        case .review:
            ReviewView(model: models.review).navigationTitle("Review")
        case .morning:
            MorningSummaryView(model: models.morning).navigationTitle("Morning")
        case .evening:
            EndOfDayPlanView(model: models.evening).navigationTitle("Evening")
        case .search:
            searchRoot
        }
    }

    // MARK: - Navigation

    /// Put `destination` on screen, wherever it belongs.
    ///
    /// The stack is *replaced* rather than appended to. A link arriving while
    /// the user is three screens deep should land them on what the link named,
    /// not on what the link named on top of wherever they happened to be.
    private func show(_ destination: Destination) {
        let route = TabRoute.route(to: destination)
        tab = route.tab
        switch route.tab {
        case .today: todayPath = route.path
        case .browse: browsePath = route.path
        case .calendar, .focus, .search: break
        }
    }

    /// The sidebar's selection binding, translated into a push.
    ///
    /// `BrowseSidebar` is shared and takes a `Destination?` selection, because
    /// that is what a macOS `NavigationSplitView` needs. On iOS selecting a
    /// row means pushing it, so the binding converts one into the other rather
    /// than the sidebar growing a second mode.
    private var browseSelection: Binding<Destination?> {
        Binding(
            get: { browsePath.last },
            set: { destination in
                guard let destination else { return }
                browsePath = [destination]
            }
        )
    }

    @FocusState private var pane: PaneFocus?

    private var escapes: ListEscapes {
        ListEscapes(
            showFocus: { tab = .focus },
            openSearch: { tab = .search },
            // No command palette on iOS: it is a keyboard surface, and a phone
            // has no keyboard to summon it from. The screens it reaches are
            // all in the tab bar or one tap into More.
            openPalette: {},
            openCheatSheet: {}
        )
    }

    private func perform(_ action: AppAction) {
        switch action {
        case .today: show(.list(.todayAll))
        case .inbox: show(.list(.inbox))
        case .searchInView, .searchGlobal: tab = .search
        case .quickCapture: openCapture()
        // The global one always means the sheet: it arrives from outside the
        // app — a widget, the Control Center control, a `sunrise://capture`
        // link — where there is no telling what is on screen.
        case .quickCaptureGlobal: surfaces.openQuickCapture()
        case .undo: Task { await models.undo.undo() }
        case .redo: Task { await models.undo.redo() }
        default:
            _ = ListCommand.perform(
                action,
                list: activeList,
                selection: rows,
                sheets: sheets,
                escapes: escapes
            )
        }
    }

    private var activeList: TaskListModel {
        tab == .search ? models.search.results : models.list
    }

    @ToolbarContentBuilder
    private var captureButton: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Button("Capture", systemImage: "square.and.pencil", action: openCapture)
                .accessibilityIdentifier("capture")
        }
    }

    /// Capture, into whichever surface this screen already has.
    ///
    /// The same rule the Mac applies for ⌘N, and for the same reason. A list
    /// that accepts capture already shows the inline bar at its top, so the
    /// honest thing is to put the keyboard in *that* field rather than slide a
    /// sheet over a field the user can already see. Where there is no bar —
    /// the calendar, focus, search, a context list — the sheet is the fallback
    /// rather than a shortcut that quietly does nothing.
    ///
    /// Presenting the sheet unconditionally was worse than untidy: the sheet's
    /// field and the inline bar's carry the same accessibility identifier, so
    /// two of them on screen at once is genuinely ambiguous — to a UI test,
    /// and to VoiceOver.
    private func openCapture() {
        if listAcceptsCapture {
            pane = .capture
        } else {
            surfaces.openQuickCapture()
        }
    }

    /// Whether the screen on show has an inline capture bar.
    private var listAcceptsCapture: Bool {
        switch tab {
        case .today:
            todayPath.isEmpty ? TaskListKind.todayAll.acceptsCapture : pushedListAcceptsCapture(todayPath)
        case .browse:
            pushedListAcceptsCapture(browsePath)
        case .calendar, .focus, .search:
            false
        }
    }

    private func pushedListAcceptsCapture(_ path: [Destination]) -> Bool {
        guard case let .list(kind)? = path.last else { return false }
        return kind.acceptsCapture
    }

    private var settingsSheet: some View {
        NavigationStack {
            AccountView(
                settings: models.settings,
                account: models.account,
                notifications: surfaces.notifications,
                deviceID: deviceID,
                authorization: surfaces.reminders?.authorization ?? .notDetermined,
                scheduledCount: surfaces.reminders?.scheduled.count ?? 0,
                signIn: signIn,
                allowNotifications: { await surfaces.reminders?.requestAuthorization() },
                keyboard: keys,
                session: session
            )
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { showingSettings = false }
                }
            }
        }
    }

    private func startSync() async {
        guard case let .connect(url, bearer) = SyncPlan(
            relayURL: models.settings.relayURL,
            accessToken: models.account.accessToken
        ) else { return }
        try? await bridge.startSync(url: url, bearer: bearer)
    }

    private func signIn() async {
        await models.account.signIn(
            issuer: models.settings.oidcIssuer,
            clientID: models.settings.oidcClientID,
            deviceID: deviceID,
            nowMs: await bridge.nowMs()
        )
    }
}

/// Every route into the app, delivered to the shell.
///
/// A `sunrise://` link, a tapped reminder, an App Intent, the Control Center
/// control and the widget all set `pendingDestination` or `pendingCommand` on
/// the shared ``AppSurfaces``; the macOS window takes the same values into its
/// sidebar selection. Split into a modifier because `body` could not be
/// type-checked with it inline.
private struct Routing: ViewModifier {
    let surfaces: AppSurfaces
    let show: (Destination) -> Void
    let perform: (AppAction) -> Void

    func body(content: Content) -> some View {
        content
            .onChange(of: surfaces.pendingDestination) { _, destination in
                guard let destination else { return }
                show(destination)
                surfaces.destinationTaken()
            }
            .onChange(of: surfaces.pendingCommand) { _, command in
                guard let command else { return }
                surfaces.commandTaken()
                perform(command)
            }
            .onChange(of: surfaces.notifications.policy) {
                Task { await surfaces.reminders?.reconcile() }
            }
    }
}

/// The long-lived work an open vault starts: sync, the reminder schedule, the
/// undo feed and the saved-view list. The same set the macOS window starts,
/// and for the same reasons.
private struct Lifecycle: ViewModifier {
    let bridge: CoreBridge
    let models: VaultModels
    let surfaces: AppSurfaces
    @Binding var deviceID: String
    let startSync: () async -> Void

    func body(content: Content) -> some View {
        content
            .task {
                deviceID = await bridge.deviceId()
                models.account.restore()
                await startSync()
            }
            .task { await models.sync.poll(from: bridge) }
            .task { await surfaces.reminders?.follow() }
            .task { await models.undo.follow() }
            .task { await models.savedViews.load() }
            .onChange(of: models.settings.relayURL) { Task { await startSync() } }
            .onChange(of: models.account.accessToken) { _, token in
                Task { await bridge.setSyncCredential(token) }
            }
    }
}

/// The capture sheet.
///
/// iOS's answer to the Mac's borderless panel: `AppSurfaces.openQuickCapture`
/// sets a flag here rather than presenting a window, because a sheet can only
/// be presented by a view that is already on screen. `.presentationDetents`
/// keeps it to the height a single field needs — a full-screen sheet for one
/// line of text is the thing that makes quick capture stop feeling quick.
private struct CaptureSheet: ViewModifier {
    let surfaces: AppSurfaces

    func body(content: Content) -> some View {
        content.sheet(
            isPresented: Binding(
                get: { surfaces.isCapturing },
                set: { if !$0 { surfaces.captureDismissed() } }
            )
        ) {
            if let capture = surfaces.capture {
                QuickCaptureView(
                    model: capture,
                    commit: { try await surfaces.commitCapture($0) },
                    dismiss: { surfaces.captureDismissed() }
                )
                // `.medium` beside the fixed height, not instead of it. The
                // small detent is the point — a full-screen sheet for one line
                // of text is what makes quick capture stop feeling quick — but
                // the content grows: one label per token the parser could not
                // place, and enough of those would push Add, the sheet's only
                // commit control, past a height nothing can scroll.
                .presentationDetents([.height(280), .medium])
                .presentationDragIndicator(.visible)
            }
        }
    }
}

/// What ``RootView`` builds once the vault is open — see the macOS twin in
/// `macOS/VaultWindow.swift` for what this name is for.
typealias VaultShell = VaultTabs
