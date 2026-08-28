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

    var body: some View {
        Form {
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

            notificationSections
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(.vertical, 8)
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
