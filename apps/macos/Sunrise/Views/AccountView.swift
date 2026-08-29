import AppKit
import SwiftUI

/// Connection and account settings.
///
/// Offline is a supported configuration, not a degraded one, so nothing here
/// is presented as required. An empty relay URL means the vault is local and
/// complete; an empty issuer means a self-host relay that does not ask.
struct AccountView: View {
    @Bindable var settings: AppSettings
    let account: AccountModel
    @Bindable var notifications: NotificationPreferences
    let deviceID: String
    let hotkey: HotkeyStatus
    /// The system's real answer, re-read rather than remembered.
    let authorization: NotificationAuthorization
    /// How many reminders the OS is holding for this Mac right now.
    let scheduledCount: Int
    let signIn: () async -> Void
    let allowNotifications: () async -> Void
    /// The keyboard preferences, for the one setting
    /// `docs/07-clients/parity-matrix.md` §Vim-mode opt-in puts on this screen.
    ///
    /// The same object the `?` cheat sheet binds to, deliberately: two entry
    /// points to one setting is a convenience, two copies of it is a bug.
    @Bindable var keyboard: KeyboardPreferences
    /// The open session, for the vault switcher and for sealing a root to a
    /// new device. Passed in by ``VaultView``, which is handed it by
    /// ``RootView``; optional only so a preview can stand this screen up
    /// without one, which is what hides the Vaults section.
    let session: SessionModel?

    @State private var pairing: PairingModel?
    @State private var newVaultName = ""
    @State private var addingVault = false
    @State private var switching = false

    var body: some View {
        Form {
            vaultSection

            Section("Sync") {
                TextField("Relay URL", text: $settings.relayURL, prompt: Text("ws://127.0.0.1:8443/sync"))
                    .textContentType(.URL)
                Text("Leave empty to work entirely on this Mac.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Account") {
                TextField("OIDC issuer", text: $settings.oidcIssuer, prompt: Text("https://issuer.example"))
                TextField("Client ID", text: $settings.oidcClientID)
                LabeledContent("This device", value: String(deviceID.prefix(16)))
                    .monospaced()
                accountRow
            }

            Section("Quick capture") {
                LabeledContent("Shortcut") {
                    HStack(spacing: 8) {
                        Text("⌘⇧N").monospaced()
                        Image(systemName: hotkey.isActive
                            ? "checkmark.circle"
                            : "exclamationmark.triangle")
                            .foregroundStyle(hotkey.isActive ? .green : .orange)
                    }
                }
                Text(hotkey.explanation)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                // Named because `docs/07-clients/desktop.md` says quick capture
                // needs it. It does not: the shortcut is registered with
                // `RegisterEventHotKey`, which reserves one combination rather
                // than observing every keystroke. Saying so is better than a
                // settings screen that quietly contradicts the documentation.
                LabeledContent("Accessibility permission") {
                    HStack(spacing: 8) {
                        Text(HotkeyCenter.accessibilityIsTrusted ? "Granted" : "Not granted")
                            .foregroundStyle(.secondary)
                        Button("Open Settings…") { HotkeyCenter.openAccessibilitySettings() }
                    }
                }
                Text("Not required for the shortcut above.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            keyboardSection

            notificationSections
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(.vertical, 8)
        .sheet(item: $pairing) { model in
            PairingView(model: model) { pairing = nil }
        }
        .alert("Add a vault", isPresented: $addingVault) {
            TextField("Name", text: $newVaultName)
            Button("Cancel", role: .cancel) { newVaultName = "" }
            Button("Add") {
                let name = newVaultName
                newVaultName = ""
                switchVault { await session?.addVault(named: name) }
            }
        } message: {
            Text(
                """
                A separate, separately encrypted vault with its own key. \
                Sunrise closes the one that is open before it opens the new one.
                """
            )
        }
    }

    /// The vaults on this Mac, and the two things you can do about them.
    ///
    /// Absent entirely when the session has no registry — the UI-test harness
    /// and the unit tests run one fixed vault, and a switcher over a list of
    /// one it cannot change would be a control that does nothing.
    @ViewBuilder
    private var vaultSection: some View {
        if let session, let vaults = session.vaults {
            Section("Vaults") {
                Picker("Open vault", selection: selection(vaults)) {
                    ForEach(vaults.vaults) { vault in
                        Text(vault.name).tag(vault.id)
                    }
                }
                .disabled(switching)
                .accessibilityIdentifier("account.vaultPicker")

                Text(
                    """
                    Only one vault is open at a time. Switching closes the one \
                    you are in — the core holds a lock on it — and then opens \
                    the other, so anything unsaved is written first.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)

                HStack(spacing: 12) {
                    Button("Add a vault…") { addingVault = true }
                        .disabled(switching)
                        .accessibilityIdentifier("account.addVault")
                    if switching {
                        ProgressView().controlSize(.small)
                    }
                    Spacer()
                    Button("Add a device…") { pairing = makePairing(session) }
                        .disabled(session.bridge == nil)
                        .accessibilityIdentifier("account.addDevice")
                }
                Text(
                    """
                    "Add a device" hands this vault's key to another Mac, after \
                    you have compared six digits on both screens.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
    }

    /// The picker writes through `SessionModel.switchTo`, never straight into
    /// the registry: selecting a vault the app has not opened yet would leave
    /// the list and the open core disagreeing about which vault this is.
    private func selection(_ vaults: VaultRegistry) -> Binding<String> {
        Binding(
            get: { vaults.selectedID },
            set: { id in
                guard let descriptor = vaults.vaults.first(where: { $0.id == id }) else { return }
                switchVault { await session?.switchTo(descriptor) }
            }
        )
    }

    private func switchVault(_ work: @escaping () async -> Void) {
        switching = true
        Task {
            await work()
            switching = false
        }
    }

    private func makePairing(_ session: SessionModel) -> PairingModel {
        PairingModel(
            intent: .addAnotherDevice,
            relayURL: settings.relayURL.trimmed,
            sealRoot: { [bridge = session.bridge] pairing in
                guard let bridge else { throw PairingUIError.noOpenVault }
                return try await bridge.sendVaultRoot(to: pairing)
            }
        )
    }

    /// `Settings → Keyboard`. One toggle, and it is the one the spec names.
    ///
    /// `docs/07-clients/parity-matrix.md` §Vim-mode opt-in calls for a settings
    /// toggle `editor.vim_mode`; the `?` cheat sheet carries the same switch,
    /// because that is where somebody asking "what are the keys" already is.
    /// Both bind to the same ``KeyboardPreferences``, so this is one setting
    /// with two entry points rather than two sources of truth — flip it here
    /// and the sheet is already showing the vim section.
    @ViewBuilder
    private var keyboardSection: some View {
        Section("Keyboard") {
            Toggle("Vim-style motions", isOn: $keyboard.vimMode)
                .accessibilityIdentifier("account.vimMode")
            Text(
                "h j k l, gg, G, u, ⌃R, / and : in any list. Additive — ⌘N, X and the "
                    + "rest keep working. Stored on this Mac only, never synced."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            LabeledContent("Shortcut reference") {
                Text("Press ? in any view").foregroundStyle(.secondary)
            }
        }
    }

    /// `Settings → Notifications`, as `docs/08-features/notifications.md`
    /// specifies it: categories on or off, the global lead time, quiet hours,
    /// and which device is primary.
    ///
    /// Per-Stream lead times are the one item on that list which is not here.
    /// They are a field on the Stream entity — they describe the work, not the
    /// machine — so they belong in the stream editor beside the rest of it,
    /// and a copy on this screen would be a second place to set one value.
    @ViewBuilder
    private var notificationSections: some View {
        Section("Notifications") {
            LabeledContent("System permission") {
                HStack(spacing: 8) {
                    Text(authorization.summary).foregroundStyle(.secondary)
                    Image(systemName: authorization.isActive
                        ? "checkmark.circle"
                        : "exclamationmark.triangle")
                        .foregroundStyle(authorization.isActive ? .green : .orange)
                    if authorization.canRequest {
                        Button("Allow…") { Task { await allowNotifications() } }
                    } else if !authorization.isActive {
                        Button("Open Settings…") { Self.openNotificationSettings() }
                    }
                }
            }
            Text(authorization.explanation)
                .font(.caption)
                .foregroundStyle(.secondary)

            Toggle("Remind me on this Mac", isOn: $notifications.isEnabled)
            Toggle("This is my primary device", isOn: $notifications.isPrimaryDevice)
            Text(
                "Only the primary device delivers reminders — the core hands the others "
                    + "nothing to schedule, so an account with four Macs still rings once."
            )
            .font(.caption)
            .foregroundStyle(.secondary)

            Picker("Remind me", selection: $notifications.leadMinutes) {
                ForEach(NotificationPreferences.leadChoices, id: \.self) { minutes in
                    Text(Self.leadLabel(minutes)).tag(minutes)
                }
            }
            Text(
                "The fallback. A lead time set on a task wins over one set on its stream, "
                    + "and either wins over this."
            )
            .font(.caption)
            .foregroundStyle(.secondary)

            LabeledContent("Scheduled now", value: "\(scheduledCount)")
                .foregroundStyle(.secondary)
        }

        Section("Quiet hours") {
            Toggle("Silence reminders overnight", isOn: $notifications.quietHoursEnabled)
            Picker("From", selection: $notifications.quietStartMinutes) { clockChoices }
                .disabled(!notifications.quietHoursEnabled)
            Picker("Until", selection: $notifications.quietEndMinutes) { clockChoices }
                .disabled(!notifications.quietHoursEnabled)
            Picker("During quiet hours", selection: $notifications.quietPolicyIsDrop) {
                Text("Hold until it ends").tag(false)
                Text("Drop them").tag(true)
            }
            .disabled(!notifications.quietHoursEnabled)
            Text(
                "A held reminder fires when the window ends, and no more than four hours "
                    + "after it was due — past that it is dropped rather than delivered late."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var clockChoices: some View {
        ForEach(NotificationPreferences.clockChoices, id: \.self) { minutes in
            Text(NotificationPreferences.clockLabel(minutesPastMidnight: minutes)).tag(minutes)
        }
    }

    private static func leadLabel(_ minutes: Int) -> String {
        minutes == 0 ? "At the scheduled time" : "\(minutes) minutes before"
    }

    /// The pane where a refused permission is granted again.
    private static func openNotificationSettings() {
        guard let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.notifications"
        ) else { return }
        NSWorkspace.shared.open(url)
    }

    @ViewBuilder
    private var accountRow: some View {
        switch account.state {
        case .signedOut:
            Button("Sign in…") { Task { await signIn() } }
                .disabled(!settings.canSignIn)
        case .awaitingBrowser:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Finish signing in in your browser…")
            }
        case let .signedIn(expiresAtMs):
            LabeledContent("Signed in") {
                HStack(spacing: 12) {
                    Text(expiry(expiresAtMs))
                        .foregroundStyle(.secondary)
                    Button("Sign out", role: .destructive) { account.signOut() }
                }
            }
        case let .failed(message):
            VStack(alignment: .leading, spacing: 6) {
                Label(message, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                Button("Try again") { Task { await signIn() } }
                    .disabled(!settings.canSignIn)
            }
        }
    }

    private func expiry(_ ms: UInt64) -> String {
        let date = Date(timeIntervalSince1970: Double(ms) / 1000)
        return "expires \(date.formatted(date: .abbreviated, time: .shortened))"
    }
}
