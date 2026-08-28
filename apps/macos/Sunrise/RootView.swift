import SwiftUI

/// What the window shows, decided by the session phase.
///
/// The four cases are kept apart on screen because they are apart in reality:
/// a first run has nothing to lose, a locked vault has everything to lose, and
/// a failure is neither.
struct RootView: View {
    @State private var session = SessionModel.standard()

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
                    VaultView(bridge: bridge)
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

    @State private var settings = AppSettings()
    @State private var account = AccountModel()
    @State private var sync = SyncStatusModel()
    @State private var browse: BrowseModel
    @State private var list: TaskListModel
    @State private var capture: CaptureModel
    @State private var search: SearchModel
    @State private var focus: FocusModel
    @State private var routines: RoutineModel
    @State private var selection: Destination? = .list(.today)
    @State private var deviceID = ""
    @State private var showingSettings = false

    init(bridge: CoreBridge) {
        self.bridge = bridge
        _browse = State(initialValue: BrowseModel(bridge: bridge))
        _list = State(initialValue: TaskListModel(bridge: bridge))
        _capture = State(initialValue: CaptureModel(bridge: bridge))
        _search = State(initialValue: SearchModel(bridge: bridge))
        _focus = State(initialValue: FocusModel(bridge: bridge))
        _routines = State(initialValue: RoutineModel(bridge: bridge))
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
                detail
            }
        }
        .toolbar {
            ToolbarItem(placement: .status) {
                SyncStatusView(presentation: sync.presentation)
            }
            ToolbarItem(placement: .primaryAction) {
                Button("Settings", systemImage: "gearshape") { showingSettings = true }
            }
        }
        .sheet(isPresented: $showingSettings) {
            VStack(alignment: .trailing, spacing: 0) {
                AccountView(
                    settings: settings,
                    account: account,
                    deviceID: deviceID,
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
        .task {
            deviceID = await bridge.deviceId()
            account.restore()
            #if DEBUG
            await DevPeerTrust.exchange(bridge: bridge)
            #endif
            await startSync()
        }
        .task { await sync.poll(from: bridge) }
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
        case .focus:
            FocusView(model: focus)
        case .routines:
            RoutinesView(model: routines)
        case .review, .none:
            ContentUnavailableView(
                selection?.title ?? "Sunrise",
                systemImage: selection?.symbol ?? "sun.max",
                description: Text("Not built yet.")
            )
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
