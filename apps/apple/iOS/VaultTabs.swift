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
    /// The screen a "Save this view…" is about, and the name being typed for
    /// it. `nil` means the sheet is closed.
    @State private var savingView: Destination?
    @State private var newViewName = ""
    @State private var importingIcal = false
    @State private var exportingIcal: IcalDocument?

    init(bridge: CoreBridge, session: SessionModel, surfaces: AppSurfaces) {
        self.bridge = bridge
        self.session = session
        self.surfaces = surfaces
        _models = State(initialValue: VaultModels(bridge: bridge, account: session.account))
    }

    var body: some View {
        tabs
            .modifier(CaptureSheet(surfaces: surfaces))
            .modifier(rowSheets)
            .sheet(isPresented: $showingSettings) { settingsSheet }
            .modifier(library)
            .modifier(Routing(surfaces: surfaces, show: show, reveal: reveal, perform: perform))
            .modifier(Lifecycle(
                bridge: bridge, session: session,
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
                        .navigationTitle(L10n.Tabs.calendar)
                        .toolbar { captureButton }
                }
            }
            Tab(AppTab.browse.title, systemImage: AppTab.browse.symbol, value: AppTab.browse) {
                NavigationStack(path: $browsePath) { browseRoot }
            }
            Tab(AppTab.focus.title, systemImage: AppTab.focus.symbol, value: AppTab.focus) {
                NavigationStack {
                    FocusView(model: models.focus)
                        .navigationTitle(L10n.Tabs.focus)
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

    /// The row sheets, the palette and the cheat sheet — the Mac's modifier, so
    /// none of them can behave differently on the two platforms.
    private var rowSheets: KeyboardSurfaces {
        KeyboardSurfaces(
            sheets: sheets,
            palette: palette,
            preferences: keys,
            list: activeList,
            hasList: showsRows,
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

    /// Saved views and iCalendar: the two shared capabilities whose only
    /// callers were `macOS/VaultWindow.swift` and the Mac's File menu, so both
    /// compiled into this product and neither was reachable from it.
    private var library: LibrarySurfaces {
        LibrarySurfaces(
            surfaces: surfaces,
            savingView: $savingView,
            newViewName: $newViewName,
            importingIcal: $importingIcal,
            exportingIcal: $exportingIcal,
            save: { name, destination in
                await models.savedViews.save(
                    name: name,
                    destination: destination,
                    query: models.search.text,
                    contexts: destination.contextNames(in: models.list.names)
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
        .navigationTitle(L10n.Tabs.today)
        .navigationDestination(for: Destination.self) { pushed(destination: $0) }
        .toolbar {
            captureButton
            savedViewsButton(for: .list(.todayAll))
        }
        .task { await models.list.show(.todayAll) }
    }

    private var browseRoot: some View {
        BrowseSidebar(model: models.browse, selection: browseSelection)
            .navigationTitle(L10n.Tabs.browse)
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
        .navigationTitle(L10n.Tabs.search)
        .toolbar {
            captureButton
            savedViewsButton(for: .search)
        }
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
            Menu(L10n.Ios.more, systemImage: "ellipsis.circle") {
                Section(L10n.Ios.sectionToday) {
                    menuLink(to: .morning)
                    menuLink(to: .evening)
                }
                Section {
                    menuLink(to: .routines)
                    menuLink(to: .review)
                }
                // The Mac's File ▸ Import / Export Calendar, which is a scene
                // command there and has no counterpart here — so it lands in
                // the same overflow the tab-less screens do. `importIcal` runs
                // the system document picker and `exportIcal` the exporter;
                // the report both produce is `IcalSurfaces`, hung on the shell
                // exactly as the Mac hangs it on its window.
                Section(L10n.Ios.sectionCalendar) {
                    Button(L10n.Ios.importCalendar, systemImage: "square.and.arrow.down") {
                        importingIcal = true
                    }
                    .disabled(surfaces.ical == nil)
                    .accessibilityIdentifier("ical.import")
                    Menu(L10n.Ios.exportCalendar, systemImage: "square.and.arrow.up") {
                        ForEach(ExportWindow.menuOrder, id: \.self) { window in
                            Button(window.menuTitle) {
                                Task { await beginIcalExport(window) }
                            }
                        }
                    }
                    .disabled(surfaces.ical == nil)
                    .accessibilityIdentifier("ical.export")
                }
                Section {
                    Button(models.undo.undoTitle, systemImage: "arrow.uturn.backward") {
                        Task { await models.undo.undo() }
                    }
                    .disabled(!models.undo.canUndo)
                    Button(L10n.Ios.settings, systemImage: "gearshape") { showingSettings = true }
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
            .toolbar {
                captureButton
                savedViewsButton(for: .list(kind))
            }
            .task { await models.list.show(kind) }
        case .calendar:
            CalendarView(model: models.calendar).navigationTitle(L10n.Tabs.calendar)
        case .focus:
            FocusView(model: models.focus).navigationTitle(L10n.Tabs.focus)
        case .routines:
            RoutinesView(model: models.routines).navigationTitle(L10n.Tabs.routines)
        case .review:
            ReviewView(model: models.review).navigationTitle(L10n.Tabs.review)
        case .morning:
            MorningSummaryView(model: models.morning).navigationTitle(L10n.Tabs.morning)
        case .evening:
            EndOfDayPlanView(model: models.evening).navigationTitle(L10n.Tabs.evening)
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

    /// Show the entity a link named, not merely the screen it lives on: a
    /// task opens its editor, the one surface that shows it whether or not
    /// the list underneath holds it. A block needs none, because
    /// ``VaultModels/reveal(_:)`` has already moved the grid onto its day.
    private func reveal(_ entity: EntityRef) {
        Task {
            guard case let .task(item)? = await models.reveal(entity) else { return }
            sheets.editing = item
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
}

// The rest of the shell: how it routes, what its toolbars offer, and the
// long-lived work it starts. An extension so the type's own body — its state,
// its `body`, its tab roots — stays short enough to read in one pass, which is
// what `type_body_length` asks for: the wiring is what grows.
extension VaultTabs {
    private var escapes: ListEscapes {
        ListEscapes(
            showFocus: { tab = .focus },
            openSearch: { perform(.searchInView) },
            openPalette: { perform(.commandPalette) },
            openCheatSheet: { perform(.cheatSheet) }
        )
    }

    private var showsRows: Bool { tab == .today || tab == .browse || tab == .search }

    /// Run an action from a key command, the palette, a list key or a link.
    private func perform(_ action: AppAction) {
        if navigate(action) || present(action) { return }
        _ = ListCommand.perform(action, list: activeList, selection: rows, sheets: sheets, escapes: escapes)
    }

    private func navigate(_ action: AppAction) -> Bool {
        switch action {
        case .today, .inbox: // and the keyboard to the rows, as the Mac does, so `?` and `X` answer
            show(.list(action == .today ? .todayAll : .inbox))
            pane = .rows
        case .morningSummary: show(.morning)
        case .endOfDay: show(.evening)
        // ⌘F keeps the query, ⌘K starts afresh; both land in the field, as the Mac does.
        case .searchInView, .searchGlobal:
            if action == .searchGlobal { models.search.clear() }
            tab = .search
            pane = .search
        case .importCalendar: importingIcal = true
        default: return false
        }
        return true
    }

    /// A phone leaves the palette and cheat sheet inert (``KeyboardClass``) but claims them, or they loop via ``escapes``.
    private func present(_ action: AppAction) -> Bool {
        let keyboard = KeyboardClass.isDesktopClass
        switch action {
        case .commandPalette where keyboard: palette.present(hasSelection: !rows.isEmpty && showsRows)
        case .cheatSheet where keyboard: showingCheatSheet = true
        case .commandPalette, .cheatSheet: break
        case .newStream: creatingStream = true
        case .quickCapture: openCapture()
        // The global one always means the sheet: it arrives from outside the
        // app — a widget, the Control Center control, a `sunrise://capture`
        // link — where there is no telling what is on screen.
        case .quickCaptureGlobal: surfaces.openQuickCapture()
        case .undo: Task { await models.undo.undo() }
        case .redo: Task { await models.undo.redo() }
        default: return false
        }
        return true
    }

    private var activeList: TaskListModel {
        tab == .search ? models.search.results : models.list
    }

    @ToolbarContentBuilder
    private var captureButton: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Button(L10n.Ios.capture, systemImage: "square.and.pencil", action: openCapture)
                .disabledUnlessEditable(.task)
                .accessibilityIdentifier("capture")
        }
    }

    /// Recall a saved view, or save the one on screen.
    ///
    /// On the Mac this is a toolbar menu on the one window; here it goes on
    /// the three screens that actually *are* a view worth saving — Today, a
    /// pushed list and Search. Putting it in Browse's overflow instead would
    /// have made "Save this view…" mean the sidebar, which is not a view the
    /// recall side can land on.
    @ToolbarContentBuilder
    private func savedViewsButton(for destination: Destination) -> some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            SavedViewsMenu(
                model: models.savedViews,
                contexts: models.list.names,
                recall: show,
                saveCurrent: { savingView = destination }
            )
            .accessibilityIdentifier("saved-views")
        }
    }

    /// Render one window of the calendar, then hand it to the exporter.
    private func beginIcalExport(_ window: ExportWindow) async {
        guard let text = await surfaces.ical?.exportDocument(window: window) else { return }
        exportingIcal = IcalDocument(text: text, filename: window.suggestedFilename)
    }

    /// Capture, into whichever surface this screen already has — the Mac's ⌘N
    /// rule. A list that accepts capture already shows the inline bar, so the
    /// keyboard goes into *that* field; where there is no bar (calendar, focus,
    /// search, a context list) the sheet is the fallback. Never both: the two
    /// fields share an accessibility identifier, so two on screen at once is
    /// ambiguous to a UI test and to VoiceOver.
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
        SettingsSheet(
            bridge: bridge, session: session,
            models: models,
            surfaces: surfaces,
            keys: keys,
            deviceID: deviceID,
            isPresented: $showingSettings
        )
    }

    private func startSync() async {
        // Registered first, so the plan below reads the id it produced (#183).
        await session.bindRelayDevice()
        let (relayURL, token) = (models.settings.relayURL, models.account.accessToken)
        guard case let .connect(url, bearer, relayDeviceID) = SyncPlan(
            relayURL: relayURL,
            accessToken: token,
            relayDeviceID: session.relayDeviceID(relayURL: relayURL, bearer: token)
        ) else { return }
        try? await bridge.startSync(url: url, bearer: bearer, relayDeviceID: relayDeviceID)
        // After the binding above, so a relay device id that registration or
        // a re-pairing just minted is the one the token is filed under.
        await BackgroundHost.shared.uploadPushToken()
    }
}

/// What ``RootView`` builds once the vault is open — see the macOS twin in
/// `macOS/VaultWindow.swift` for what this name is for.
typealias VaultShell = VaultTabs
