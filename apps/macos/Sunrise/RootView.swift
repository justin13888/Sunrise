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

/// The unlocked app. A placeholder until Today and Inbox land.
struct VaultView: View {
    let bridge: CoreBridge

    @State private var settings = AppSettings()
    @State private var account = AccountModel()
    @State private var sync = SyncStatusModel()
    @State private var deviceID = ""
    @State private var showingSettings = false

    var body: some View {
        VStack(spacing: 0) {
            SyncWarningBanner(presentation: sync.presentation)
            ContentUnavailableView(
                "Vault open",
                systemImage: "checkmark.seal",
                description: Text("Today and Inbox arrive next.")
            )
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
