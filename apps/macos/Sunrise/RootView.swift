import SwiftUI

/// What the window shows, decided by the session phase.
///
/// The four cases are kept apart on screen because they are apart in reality:
/// a first run has nothing to lose, a locked vault has everything to lose, and
/// a failure is neither.
struct RootView: View {
    let session: SessionModel
    let surfaces: AppSurfaces

    var body: some View {
        Group {
            switch session.phase {
            case .starting:
                ProgressView("Opening your vault…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .firstRun:
                OnboardingView(create: session.createVault)
            case let .locked(reason):
                LockedView(reason: reason, retry: session.start)
            case .unlocked:
                if let bridge = session.bridge {
                    VaultView(bridge: bridge, surfaces: surfaces)
                        // The menu bar item, the capture panel and the
                        // reminder schedule need the same open vault this
                        // window is using, and this is the first moment there
                        // is one.
                        //
                        // Keyed on the bridge's identity: switching vaults
                        // opens a different `Core`, and the three surfaces
                        // above have to be rebuilt against it. Without the id
                        // this would not re-run if SwiftUI ever reused the
                        // view — and the surfaces would go on writing to a
                        // core that has been shut down.
                        .task(id: ObjectIdentifier(bridge)) {
                            surfaces.attach(bridge: bridge)
                            await surfaces.reminders?.start()
                        }
                }
            case let .failed(message):
                ContentUnavailableView(
                    "Sunrise could not start",
                    systemImage: "exclamationmark.triangle",
                    description: Text(message)
                )
            }
        }
        .task { await session.start() }
        // Every `sunrise://` link the OS hands this process arrives here.
        // Attached to the window's root rather than to a scene that may not
        // exist: a link that arrives while Sunrise is closed opens this window
        // to deliver it, which is exactly what a tapped reminder should do.
        //
        // Anything the parser refuses is dropped in silence, per
        // `docs/07-clients/interaction-patterns.md`.
        .onOpenURL { url in
            guard let link = DeepLink(url: url) else { return }
            surfaces.open(link)
        }
    }
}

/// The unlocked app.
struct VaultView: View {
    let bridge: CoreBridge
    let surfaces: AppSurfaces

    @State private var settings = AppSettings()
    @State private var account = AccountModel()
    @State private var sync = SyncStatusModel()
    @State private var browse: BrowseModel
    @State private var list: TaskListModel
    @State private var capture: CaptureModel
    @State private var search: SearchModel
    @State private var calendar: CalendarModel
    @State private var focus: FocusModel
    @State private var routines: RoutineModel
    @State private var review: ReviewModel
    @State private var morning: MorningSummaryModel
    @State private var evening: EndOfDayPlanModel
    @State private var undo: UndoModel
    @State private var savedViews = SavedViewsModel()
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
    @FocusState private var pane: PaneFocus?

    init(bridge: CoreBridge, surfaces: AppSurfaces) {
        self.bridge = bridge
        self.surfaces = surfaces
        _browse = State(initialValue: BrowseModel(bridge: bridge))
        _list = State(initialValue: TaskListModel(bridge: bridge))
        _capture = State(initialValue: CaptureModel(bridge: bridge))
        _search = State(initialValue: SearchModel(bridge: bridge))
        _calendar = State(initialValue: CalendarModel(bridge: bridge))
        _focus = State(initialValue: FocusModel(bridge: bridge))
        _routines = State(initialValue: RoutineModel(bridge: bridge))
        _review = State(initialValue: ReviewModel(bridge: bridge))
        _morning = State(initialValue: MorningSummaryModel(bridge: bridge))
        _evening = State(initialValue: EndOfDayPlanModel(bridge: bridge))
        _undo = State(initialValue: UndoModel(bridge: bridge))
    }

    var body: some View {
        NavigationSplitView {
            BrowseSidebar(model: browse, selection: $selection)
        } detail: {
            VStack(spacing: 0) {
                SyncWarningBanner(presentation: sync.presentation)
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
                    allowNotifications: { await surfaces.reminders?.requestAuthorization() }
                )
                Button("Done") { showingSettings = false }
                    .keyboardShortcut(.defaultAction)
                    .padding()
            }
        }
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
        // A deep link, a tapped notification or ⌘⌥M asked for a screen. The
        // window is the only thing that can grant that, so it is the thing
        // that takes the request and clears it.
        .onChange(of: surfaces.pendingDestination) { _, destination in
            guard let destination else { return }
            selection = destination
            surfaces.destinationTaken()
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
            #if DEBUG
            await DevPeerTrust.exchange(bridge: bridge)
            #endif
            await startSync()
        }
        .task { await sync.poll(from: bridge) }
        // The schedule is only correct until the next write. A task created on
        // the phone and synced here has to reach this Mac's notification
        // centre without anybody opening a screen.
        .task { await surfaces.reminders?.poll() }
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
        default: return false
        }
        return true
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
        guard case let .list(kind)? = selection else { return [] }
        switch kind {
        case let .today(contexts): return contexts.compactMap { list.names.contexts[$0] }
        case let .context(_, name): return [name]
        case .inbox, .stream, .search: return []
        }
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

/// A dismissible line of explanation. Not an error: what it reports has
/// already happened.
struct NoteBanner: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "info.circle")
            Text(text).font(.callout)
            Spacer()
            Button("Dismiss", systemImage: "xmark", action: dismiss)
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.quaternary.opacity(0.5))
    }
}
