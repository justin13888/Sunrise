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

    @Environment(\.scenePhase) private var scenePhase
    /// What the wind-down row shows and whether the Sleep Focus explanation
    /// is up. A model rather than two `@State` values so a test can drive the
    /// same wiring this view runs, against a fake notification center.
    @State private var windDown: WindDownDeliveryModel

    /// `center` is where the time-sensitive setting is read from; the system's
    /// unless a test host answers for it.
    init(
        policy: CatalogPolicyItem,
        center: any NotificationCenterClient = SystemNotificationCenter(),
        set: @escaping (NotificationKind, Bool) -> Void
    ) {
        self.policy = policy
        self.set = set
        _windDown = State(initialValue: WindDownDeliveryModel(center: center))
    }

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
            await windDown.refresh(in: scenePhase)
        }
        .alert(L10n.Account.NotificationKinds.sleepFocusTitle, isPresented: $windDown.explainingSleepFocus) {
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
                        windDown.switchFlipped(info.kind, wasOn: wasOn, isOn: isOn)
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
            if let timeSensitive = windDown.timeSensitive {
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
        if let caption = windDown.timeSensitive?.caption {
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

/// The wind-down row's state in ``NotificationKindsSection``: the
/// time-sensitive setting it shows, and whether the Sleep Focus explanation is
/// up (`docs/08-features/notifications.md` §Wind-down).
///
/// The view only forwards to it, so the tests drive this with a fake
/// ``NotificationCenterClient`` and cover the same wiring the screen runs.
@MainActor
@Observable
final class WindDownDeliveryModel {
    /// `nil` until the first read answers, so the row never claims a state it
    /// has not seen.
    private(set) var timeSensitive: TimeSensitiveDelivery?
    var explainingSleepFocus = false

    private let center: any NotificationCenterClient

    init(center: any NotificationCenterClient) {
        self.center = center
    }

    /// Re-read the setting when the app is at the front, which is every return
    /// from the System Settings page the explanation's button opens; any other
    /// phase keeps the last answer.
    func refresh(in phase: ScenePhase) async {
        guard phase == .active else { return }
        timeSensitive = await center.timeSensitiveDelivery()
    }

    /// A kind's switch moved; raise the explanation when
    /// ``TimeSensitiveDelivery/explainsSleepFocus(_:wasOn:isOn:)`` says so.
    func switchFlipped(_ kind: NotificationKind, wasOn: Bool, isOn: Bool) {
        if TimeSensitiveDelivery.explainsSleepFocus(kind, wasOn: wasOn, isOn: isOn) {
            explainingSleepFocus = true
        }
    }
}
