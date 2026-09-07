import SwiftUI

/// The unlocked app.
struct VaultWindow: View {
    let bridge: CoreBridge
    /// Carried through to Settings, which is where a vault is switched and
    /// where a second Mac is handed this vault's key.
    let session: SessionModel
    let surfaces: AppSurfaces

    /// Every model this window draws, built once from the bridge.
    ///
    /// Shared with the iOS shell — see ``VaultModels``. The fifteen names
    /// below are unwrapped from it rather than declared here so that the two
    /// shells cannot drift: adding a model means adding it in one place, and
    /// both get it.
    @State private var models: VaultModels

    private var settings: AppSettings { models.settings }
    private var account: AccountModel { models.account }
    private var sync: SyncStatusModel { models.sync }
    private var browse: BrowseModel { models.browse }
    private var list: TaskListModel { models.list }
    private var capture: CaptureModel { models.capture }
    private var search: SearchModel { models.search }
    private var calendar: CalendarModel { models.calendar }
    private var focus: FocusModel { models.focus }
    private var routines: RoutineModel { models.routines }
    private var review: ReviewModel { models.review }
    private var morning: MorningSummaryModel { models.morning }
    private var evening: EndOfDayPlanModel { models.evening }
    private var undo: UndoModel { models.undo }
    private var savedViews: SavedViewsModel { models.savedViews }

    @State private var savingView = false
    @State private var newViewName = ""
    @State private var selection: Destination? = .list(.todayAll)
    @State private var deviceID = ""
    @State private var showingSettings = false

    // The keyboard. One selection and one set of row sheets for the whole
    // window, shared by Today, a stream, Search and the command palette — so
    // "Schedule…" means the same thing however it was asked for, and the rows
    // it acts on are the rows the user can see highlighted.
    @State private var keys = KeyboardPreferences()
    @State private var rows = ListSelection()
    @State private var sheets = RowSheets()
    @State private var palette = CommandPaletteModel()
    @State private var showingCheatSheet = false
    @State private var creatingStream = false
    /// Off unless the first-run coachmark was ticked — see ``KeyboardTips``.
    @State private var tips = KeyboardTips()
    @FocusState private var pane: PaneFocus?

    init(bridge: CoreBridge, session: SessionModel, surfaces: AppSurfaces) {
        self.bridge = bridge
        self.session = session
        self.surfaces = surfaces
        _models = State(initialValue: VaultModels(bridge: bridge))
    }

    var body: some View {
        NavigationSplitView {
            BrowseSidebar(model: browse, selection: $selection)
        } detail: {
            VStack(spacing: 0) {
                SyncWarningBanner(presentation: sync.presentation)
                // The other half of the first-run coachmark. Dismissing turns
                // the preference off rather than hiding one instance: someone
                // who has read it once has read it.
                if tips.isEnabled {
                    NoteBanner(text: KeyboardTips.hint) { tips.isEnabled = false }
                }
                if let note = browse.undoNote {
                    NoteBanner(text: note) { browse.dismissUndoNote() }
                }
                if let note = savedViews.recallNote {
                    NoteBanner(text: note) { savedViews.dismissRecallNote() }
                }
                if let action = undo.lastAction {
                    NoteBanner(text: action) { undo.dismissLastAction() }
                }
                detail
            }
        }
        .toolbar {
            ToolbarItemGroup {
                UndoMenu(model: undo)
                SavedViewsMenu(
                    model: savedViews,
                    contexts: list.names,
                    recall: { selection = $0 },
                    saveCurrent: { savingView = true }
                )
            }
            ToolbarItem(placement: .status) {
                SyncStatusView(presentation: sync.presentation)
            }
            ToolbarItem(placement: .primaryAction) {
                Button("Settings", systemImage: "gearshape") { showingSettings = true }
            }
        }
        .sheet(isPresented: $savingView) {
            SaveViewSheet(
                name: $newViewName,
                summary: selection?.title ?? "Today"
            ) {
                guard let selection else { return }
                await savedViews.save(
                    name: newViewName,
                    destination: selection,
                    query: search.text,
                    contexts: currentContextNames
                )
                newViewName = ""
            }
        }
        .sheet(isPresented: $showingSettings) {
            VStack(alignment: .trailing, spacing: 0) {
                AccountView(
                    settings: settings,
                    account: account,
                    notifications: surfaces.notifications,
                    deviceID: deviceID,
                    hotkey: surfaces.hotkeyStatus,
                    authorization: surfaces.reminders?.authorization ?? .notDetermined,
                    scheduledCount: surfaces.reminders?.scheduled.count ?? 0,
                    signIn: signIn,
                    allowNotifications: { await surfaces.reminders?.requestAuthorization() },
                    keyboard: keys,
                    session: session
                )
                Button("Done") { showingSettings = false }
                    .keyboardShortcut(.defaultAction)
                    .padding()
            }
        }
        // The File menu's import lands here: the report is a sheet on the
        // window, because the menu that started it lives in a scene that has
        // nowhere to put one.
        .modifier(IcalSurfaces(model: surfaces.ical))
        .modifier(
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
                    await browse.createStream(
                        name: name,
                        color: edit.color,
                        cadence: edit.reviewCadence
                    )
                }
            )
        )
        // `?` from a view with no rows — Calendar, Focus, the two briefs. The
        // spec says "in any view", and a cheat sheet that only appeared where
        // the keys already worked would be the wrong way round.
        //
        // Refused while a field has the keyboard. A focused `TextField` should
        // consume the press before it reaches this far, but "should" is not a
        // good enough guarantee for a key that would otherwise interrupt
        // someone mid-sentence.
        .onKeyPress("?") {
            guard pane != .capture, pane != .search else { return .ignored }
            showingCheatSheet = true
            return .handled
        }
        // A menu item or the palette asked for something only the window can
        // grant. Taken and cleared the same way `pendingDestination` is, so
        // pressing ⌘⇧P twice opens the palette twice.
        .onChange(of: surfaces.pendingCommand) { _, command in
            guard let command else { return }
            surfaces.commandTaken()
            perform(command)
        }
        .onChange(of: selection) { _, destination in
            guard case let .list(kind)? = destination else { return }
            Task { await list.show(kind) }
        }
        // Keep the File menu's two print items in step with what is on screen;
        // the menu lives in a scene that cannot see `selection`.
        .modifier(
            PrintMenuSync(surfaces: surfaces, destination: selection, reviewTab: review.tab)
        )
        // A deep link, a tapped notification or ⌘⌥M asked for a screen. The
        // window is the only thing that can grant that, so it is the thing
        // that takes the request and clears it.
        .onChange(of: surfaces.pendingDestination) { _, destination in
            guard let destination else { return }
            selection = destination
            surfaces.destinationTaken()
        }
        // …and which *thing* on that screen. Taken separately because it is a
        // read: the sidebar moves now, and the entity's own editor opens when
        // the vault has answered. A block needs no second surface — by then
        // `reveal` has moved the grid onto its day, which is where it is.
        .onChange(of: surfaces.pendingReveal) { _, entity in
            guard let entity else { return }
            surfaces.revealTaken()
            Task {
                guard case let .task(item)? = await models.reveal(entity) else { return }
                sheets.editing = item
            }
        }
        // Settings that change what the OS is holding: a new quiet window, a
        // different lead time, or this Mac ceasing to be the primary device —
        // which must make it go quiet, not merely stop adding.
        .onChange(of: surfaces.notifications.policy) {
            Task { await surfaces.reminders?.reconcile() }
        }
        .task {
            deviceID = await bridge.deviceId()
            account.restore()
            await startSync()
        }
        .task { await sync.poll(from: bridge) }
        // The schedule is only correct until the next write. A task created on
        // the phone and synced here has to reach this Mac's notification
        // centre without anybody opening a screen — so this follows the change
        // feed rather than waking on a timer.
        .task { await surfaces.reminders?.follow() }
        .task { await undo.follow() }
        .task { await savedViews.load() }
        .onChange(of: settings.relayURL) { Task { await startSync() } }
        .onChange(of: account.accessToken) { _, token in
            Task { await bridge.setSyncCredential(token) }
        }
    }

    @ViewBuilder
    private var detail: some View {
        switch selection {
        case .list:
            TaskListView(
                model: list,
                capture: capture,
                selection: rows,
                sheets: sheets,
                preferences: keys,
                escapes: escapes,
                focus: $pane
            )
        case .search:
            SearchView(
                model: search,
                selection: rows,
                sheets: sheets,
                preferences: keys,
                escapes: escapes,
                focus: $pane
            )
        case .calendar:
            CalendarView(model: calendar)
        case .focus:
            FocusView(model: focus)
        case .routines:
            RoutinesView(model: routines)
        case .review:
            ReviewView(model: review)
        case .morning:
            MorningSummaryView(model: morning)
        case .evening:
            EndOfDayPlanView(model: evening)
        case .none:
            ContentUnavailableView(
                "Sunrise",
                systemImage: "sun.max",
                description: Text("Pick something on the left.")
            )
        }
    }

    // MARK: - The keyboard

    /// The list the row keys act on.
    ///
    /// Search draws the same rows through the same model, so it is a task list
    /// as far as the keyboard is concerned — and `X` on a search result has to
    /// complete it, not do nothing.
    private var activeList: TaskListModel {
        if case .search = selection { return search.results }
        return list
    }

    /// Whether what is on screen has rows at all. Decides what the cheat sheet
    /// prints and whether the palette offers the row commands.
    private var showsRows: Bool {
        switch selection {
        case .list, .search: true
        default: false
        }
    }

    private var escapes: ListEscapes {
        ListEscapes(
            showFocus: { selection = .focus },
            openSearch: { perform(.searchInView) },
            openPalette: { perform(.commandPalette) },
            openCheatSheet: { showingCheatSheet = true }
        )
    }

    /// Run an application-scope action.
    ///
    /// The one place a menu item, a `sunrise://` link and the command palette
    /// all end up, so a command cannot behave differently depending on how it
    /// was asked for. Row-scoped actions are `ListCommand`'s, not this
    /// method's — they need a list and a selection, and this has neither.
    private func perform(_ action: AppAction) {
        if navigate(action) { return }
        if present(action) { return }
        // What is left is a row action arriving from the command palette. It
        // needs the list and the selection, which `ListCommand` has and this
        // does not — and running it through the same function the key handler
        // uses is why `X` and "Mark Done" cannot mean different things.
        _ = ListCommand.perform(
            action,
            list: activeList,
            selection: rows,
            sheets: sheets,
            escapes: escapes
        )
    }

    /// The commands that change what the window is showing.
    private func navigate(_ action: AppAction) -> Bool {
        switch action {
        case .today:
            selection = .list(.todayAll)
            pane = .rows
        case .inbox:
            selection = .list(.inbox)
            pane = .rows
        case .searchInView:
            // This client has one search surface. ⌘F carries the query over —
            // "keep looking" — and ⌘K starts a fresh one.
            selection = .search
            pane = .search
        case .searchGlobal:
            search.clear()
            selection = .search
            pane = .search
        default: return false
        }
        return true
    }

    /// The commands that put something *over* the window, plus the two that go
    /// straight to the undo stack.
    private func present(_ action: AppAction) -> Bool {
        switch action {
        case .quickCaptureGlobal: surfaces.openQuickCapture()
        case .quickCapture: openCapture()
        case .commandPalette: palette.present(hasSelection: !rows.isEmpty && showsRows)
        case .newStream: creatingStream = true
        case .cheatSheet: showingCheatSheet = true
        case .undo: Task { await undo.undo() }
        case .redo: Task { await undo.redo() }
        case .printView, .exportPDF: PrintCommand.run(action, document: printable)
        default: return false
        }
        return true
    }

    /// What ⌘P and "Export as PDF…" would produce here.
    private var printable: PrintDocument? {
        PrintDocument.forDestination(
            selection,
            list: activeList,
            search: search,
            calendar: calendar,
            review: review
        )
    }

    /// ⌘N. In-app capture is the bar at the top of a list; where there is no
    /// bar — Search, a context list, the Calendar — the panel is the honest
    /// fallback rather than a shortcut that quietly does nothing.
    private func openCapture() {
        if case let .list(kind)? = selection, kind.acceptsCapture {
            pane = .capture
        } else {
            surfaces.openQuickCapture()
        }
    }

    /// The context names a saved view of the current screen should carry.
    ///
    /// Only a filtered Today or a context list has any; everything else saves
    /// no filter rather than one it could not honour on recall.
    private var currentContextNames: [String] {
        selection?.contextNames(in: list.names) ?? []
    }

    /// Start (or re-point) the sync driver. Safe to call again: the driver
    /// picks a new credential up on its next connect, and a repeated start
    /// against the same URL is refused by the core rather than doubled.
    private func startSync() async {
        guard case let .connect(url, bearer) = SyncPlan(
            relayURL: settings.relayURL,
            accessToken: account.accessToken
        ) else { return }
        try? await bridge.startSync(url: url, bearer: bearer)
    }

    private func signIn() async {
        await account.signIn(
            issuer: settings.oidcIssuer,
            clientID: settings.oidcClientID,
            deviceID: deviceID,
            nowMs: await bridge.nowMs()
        )
    }
}

/// What ``RootView`` builds once the vault is open.
///
/// The one name the shared root knows. macOS resolves it to the split-view
/// window above; iOS resolves it to the tab shell in `iOS/VaultTabs.swift`.
/// Both take the same three collaborators and own the same models — the
/// difference is entirely navigation, which is the one thing
/// `docs/07-clients/shared-ui-system.md` says each platform owns outright.
typealias VaultShell = VaultWindow
