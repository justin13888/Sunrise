import Foundation

@testable import Sunrise

/// Builds an ``AccountView`` for a test, with the one argument that does not
/// exist on both platforms supplied where it does.
///
/// `hotkey:` is macOS-only — iOS has no global hotkey for a settings row to
/// report on — and Swift cannot gate a single argument of a call with `#if`,
/// so without this every test that stands the settings screen up would have to
/// write the call out twice. It is written out twice here instead, once.
@MainActor
enum AccountViewFixture {
    // swiftlint:disable:next function_parameter_count
    static func make(
        settings: AppSettings,
        account: AccountModel,
        notifications: NotificationPreferences,
        deviceID: String,
        authorization: NotificationAuthorization,
        scheduledCount: Int,
        signIn: @escaping () async -> Void,
        allowNotifications: @escaping () async -> Void,
        keyboard: KeyboardPreferences,
        session: SessionModel?
    ) -> AccountView {
        #if os(macOS)
        AccountView(
            settings: settings,
            account: account,
            notifications: notifications,
            deviceID: deviceID,
            hotkey: .active,
            authorization: authorization,
            scheduledCount: scheduledCount,
            signIn: signIn,
            allowNotifications: allowNotifications,
            keyboard: keyboard,
            session: session
        )
        #else
        AccountView(
            settings: settings,
            account: account,
            notifications: notifications,
            deviceID: deviceID,
            authorization: authorization,
            scheduledCount: scheduledCount,
            signIn: signIn,
            allowNotifications: allowNotifications,
            keyboard: keyboard,
            session: session
        )
        #endif
    }
}
