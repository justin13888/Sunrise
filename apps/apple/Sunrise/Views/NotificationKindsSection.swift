import SwiftUI

/// Settings → Notifications: one switch per kind of the catalog, in catalog
/// order (`docs/08-features/notifications.md` §Settings → Notifications).
///
/// The rows are the core's catalog and the switches its policy, so a kind the
/// core adds appears here without an edit. A security alert has no switch —
/// the catalog gives it no key — and says it is always on instead. On a
/// device with a keyboard, a row whose notification opens something the
/// keymap binds shows that key, as every other surface does
/// (`docs/08-features/keyboard.md` Rule 1).
///
/// Wind-down carries two things more (§Wind-down): whether this device lets
/// Sunrise send time-sensitive notifications, read from the system each time
/// the screen appears or the app comes back to the front, and, when the
/// switch is turned on, an explanation of the Sleep Focus that offers the
/// system settings where Sunrise is let through it.
struct NotificationKindsSection: View {
    let policy: CatalogPolicyItem
    let set: (NotificationKind, Bool) -> Void
    /// Where the time-sensitive setting is read from. A parameter so a test
    /// host can answer for the system.
    var center: any NotificationCenterClient = SystemNotificationCenter()

    @Environment(\.scenePhase) private var scenePhase
    /// `nil` until the first read answers, so the row never claims a state
    /// it has not seen.
    @State private var timeSensitive: TimeSensitiveDelivery?
    @State private var explainingSleepFocus = false

    var body: some View {
        Section {
            ForEach(NotificationCatalog.kinds, id: \.tag) { info in
                row(info)
            }
        } header: {
            Text(L10n.Account.NotificationKinds.title)
        } footer: {
            Text(L10n.Account.NotificationKinds.caption)
        }
        // Re-read on every return to the front: the button below sends the
        // user to System Settings, and the row must say what they did there.
        .task(id: scenePhase) {
            guard scenePhase == .active else { return }
            timeSensitive = await center.timeSensitiveDelivery()
        }
        .alert(L10n.Account.NotificationKinds.sleepFocusTitle, isPresented: $explainingSleepFocus) {
            Button(TimeSensitiveDelivery.focusSettingsLabel) { Self.openFocusSettings() }
            Button(L10n.Account.NotificationKinds.notNow, role: .cancel) {}
        } message: {
            Text(L10n.Account.NotificationKinds.sleepFocusMessage)
        }
    }

    @ViewBuilder
    private func row(_ info: NotificationKindInfo) -> some View {
        let keys = KeyboardClass.isDesktopClass ? Self.keys(for: info.kind) : ""
        if info.enabledKey == nil {
            LabeledContent(Self.title(of: info.kind)) {
                Text(L10n.Account.NotificationKinds.alwaysOn).foregroundStyle(.secondary)
            }
            .accessibilityIdentifier("notifications.kind.\(info.tag)")
        } else {
            VStack(alignment: .leading, spacing: 2) {
                Toggle(isOn: Binding(
                    get: { NotificationCatalog.isOn(info.kind, in: policy) },
                    set: { isOn in
                        let wasOn = NotificationCatalog.isOn(info.kind, in: policy)
                        set(info.kind, isOn)
                        if TimeSensitiveDelivery.explainsSleepFocus(info.kind, wasOn: wasOn, isOn: isOn) {
                            explainingSleepFocus = true
                        }
                    }
                )) {
                    HStack {
                        Text(Self.title(of: info.kind))
                        if !keys.isEmpty {
                            Spacer()
                            Text(keys).font(.caption.monospaced()).foregroundStyle(.secondary)
                        }
                    }
                }
                .accessibilityIdentifier("notifications.kind.\(info.tag)")
                if info.kind == .windDown {
                    Text(L10n.Account.NotificationKinds.windDownCaption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    windDownDelivery
                }
            }
        }
    }

    /// The time-sensitive state, under the wind-down switch it decides, and
    /// the way to the settings that let wind-down through a Sleep Focus.
    @ViewBuilder
    private var windDownDelivery: some View {
        LabeledContent(L10n.Account.NotificationKinds.timeSensitive) {
            if let timeSensitive {
                HStack(spacing: 8) {
                    Text(timeSensitive.summary).foregroundStyle(.secondary)
                    Image(systemName: timeSensitive.isAllowed
                        ? "checkmark.circle"
                        : "exclamationmark.triangle")
                        .foregroundStyle(timeSensitive.isAllowed ? .green : .orange)
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .accessibilityIdentifier("notifications.timeSensitive")
        if let caption = timeSensitive?.caption {
            Text(caption)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        // Borderless, because this sits in the same form row as the switch:
        // a plain button there makes the whole row its tap target on iOS,
        // and a tap meant for the switch would leave the app.
        Button(TimeSensitiveDelivery.focusSettingsLabel) { Self.openFocusSettings() }
            .buttonStyle(.borderless)
            .accessibilityIdentifier("notifications.focusSettings")
    }

    private static func openFocusSettings() {
        guard let url = TimeSensitiveDelivery.focusSettingsURL else { return }
        Platform.openExternal(url)
    }

    private static func title(of kind: NotificationKind) -> String { kind.settingsTitle }

    private static func keys(for kind: NotificationKind) -> String {
        kind.keyAction.map(Keymap.shortcutLabel(for:)) ?? ""
    }
}
