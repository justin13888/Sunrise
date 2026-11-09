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
struct NotificationKindsSection: View {
    let policy: CatalogPolicyItem
    let set: (NotificationKind, Bool) -> Void

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
                    set: { set(info.kind, $0) }
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
                }
            }
        }
    }

    private static func title(of kind: NotificationKind) -> String { kind.settingsTitle }

    private static func keys(for kind: NotificationKind) -> String {
        kind.keyAction.map(Keymap.shortcutLabel(for:)) ?? ""
    }
}
