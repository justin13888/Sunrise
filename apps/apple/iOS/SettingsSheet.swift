import SwiftUI

// The Settings sheet ``VaultTabs`` presents from More, in a file of its own
// for the reason `VaultSurfaces.swift` gives: the shell was past the length
// this project lints for. A view rather than an extension member, because an
// extension in another file cannot read the shell's private state; the shell
// hands it what it shows instead. Nested in ``VaultTabs`` so a name this
// general stays the shell's and not the module the Mac's sources share.
extension VaultTabs {
    /// The shared ``AccountView`` in a sheet, with the sign-in it starts.
    struct SettingsSheet: View {
        let bridge: CoreBridge
        let session: SessionModel
        let models: VaultModels
        let surfaces: AppSurfaces
        let keys: KeyboardPreferences
        let deviceID: String
        @Binding var isPresented: Bool

        var body: some View {
            NavigationStack {
                AccountView(
                    settings: models.settings,
                    account: models.account,
                    notifications: surfaces.notifications,
                    deviceID: deviceID,
                    authorization: surfaces.reminders?.authorization ?? .notDetermined,
                    scheduledCount: surfaces.reminders?.scheduled.count ?? 0,
                    signIn: signIn,
                    allowNotifications: { await surfaces.reminders?.requestAuthorization() },
                    keyboard: keys,
                    session: session,
                    devices: models.devices
                )
                .navigationTitle(L10n.Settings.title)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button(L10n.Action.done) { isPresented = false }
                    }
                }
            }
        }

        private func signIn() async {
            await models.account.signIn(
                issuer: models.settings.oidcIssuer,
                clientID: models.settings.oidcClientID,
                deviceID: deviceID,
                nowMs: await bridge.nowMs()
            )
        }
    }
}
