import SwiftUI

/// Connection and account settings.
///
/// Offline is a supported configuration, not a degraded one, so nothing here
/// is presented as required. An empty relay URL means the vault is local and
/// complete; an empty issuer means a self-host relay that does not ask.
struct AccountView: View {
    @Bindable var settings: AppSettings
    let account: AccountModel
    let deviceID: String
    let signIn: () async -> Void

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
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(.vertical, 8)
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
