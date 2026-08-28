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
                        // The menu bar item and the capture panel need the same
                        // open vault this window is using, and this is the
                        // first moment there is one.
                        .task { surfaces.attach(bridge: bridge) }
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
                    saveCurrent: { savingView = true },
                    canSaveCurrent: selection?.isSaveable ?? false
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
                    deviceID: deviceID,
                    hotkey: surfaces.hotkeyStatus,
                    signIn: signIn
                )
                Button("Done") { showingSettings = false }
                    .keyboardShortcut(.defaultAction)
                    .padding()
            }
        }
        .onChange(of: selection) { _, destination in
            guard case let .list(kind)? = destination else { return }
            Task { await list.show(kind) }
        }
        // ⌘⌥M asked for a screen. The window is the only thing that can grant
        // that, so it is the thing that takes the request and clears it.
        .onChange(of: surfaces.pendingDestination) { _, destination in
            guard let destination else { return }
            selection = destination
            surfaces.destinationTaken()
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
            TaskListView(model: list, capture: capture)
        case .search:
            SearchView(model: search)
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
