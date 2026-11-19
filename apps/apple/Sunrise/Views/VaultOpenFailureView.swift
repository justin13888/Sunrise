import SwiftUI

/// A vault that exists and would not open, with the ways out the core offers
/// for it (#1115).
///
/// Every button is a remedy ``VaultRepairModel`` holds, in the order the core
/// lists them; this screen decides none of them. Like ``LockedView`` it never
/// offers a new vault: the data on disk is what is at stake. A remedy that
/// does not work is reported above the list, and the list stays.
struct VaultOpenFailureView: View {
    let model: VaultRepairModel

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                Image(systemName: "externaldrive.badge.exclamationmark")
                    .font(.system(size: 44))
                    .foregroundStyle(.orange)
                Text(L10n.VaultOpen.title)
                    .font(.title.weight(.semibold))
                    .multilineTextAlignment(.center)
                Text(summary)
                    .font(.callout)
                    .multilineTextAlignment(.center)
                Text(model.message)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .multilineTextAlignment(.center)
                    .accessibilityIdentifier("vaultOpen.message")

                if let failure = model.failure {
                    Label(
                        L10n.VaultOpen.remedyFailed(remedy: title(failure.remedy), reason: failure.reason),
                        systemImage: "exclamationmark.circle"
                    )
                    .font(.callout)
                    .foregroundStyle(.red)
                    .accessibilityIdentifier("vaultOpen.failure")
                }

                VStack(alignment: .leading, spacing: 14) {
                    ForEach(model.remedies, id: \.self) { remedy in
                        row(remedy)
                    }
                }
                .padding(.top, 8)
            }
            .frame(maxWidth: 480)
            .padding(40)
            .frame(maxWidth: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var summary: String {
        if model.isFatal { return L10n.VaultOpen.fatal }
        return switch model.problem {
        case .damaged: L10n.VaultOpen.damaged
        case .backupFailed: L10n.VaultOpen.backupFailed
        case .migrationFailed: L10n.VaultOpen.migrationFailed
        case .tooNew: L10n.VaultOpen.tooNew
        case .tooOld: L10n.VaultOpen.tooOld
        case .busy: L10n.VaultOpen.busy
        case .other: L10n.VaultOpen.fatal
        }
    }

    @ViewBuilder
    private func row(_ remedy: OpenRemedy) -> some View {
        if remedy == .updateApp {
            Label(L10n.VaultOpen.update, systemImage: "arrow.down.app")
                .font(.callout)
                .accessibilityIdentifier("vaultOpen.updateApp")
        } else {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Button(title(remedy)) { Task { await model.perform(remedy) } }
                        .buttonStyle(.borderedProminent)
                        .tint(remedy == model.remedies.first ? .accentColor : .secondary)
                        .controlSize(.large)
                        .disabled(model.running != nil)
                        .accessibilityIdentifier("vaultOpen.\(remedy)")
                    if model.running == remedy {
                        ProgressView().controlSize(.small)
                    }
                    if remedy == .sendDiagnostics, model.diagnosticsCopied {
                        Text(L10n.VaultOpen.diagnosticsCopied)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                if let caption = caption(remedy) {
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    private func title(_ remedy: OpenRemedy) -> String {
        switch remedy {
        case .tryAgain: L10n.Action.tryAgain
        case .repair: L10n.VaultOpen.repair
        case .restoreBackup: L10n.VaultOpen.restore
        case .recoverFromCode: L10n.VaultOpen.recover
        case .updateApp: L10n.VaultOpen.update
        case .sendDiagnostics: L10n.VaultOpen.diagnostics
        }
    }

    private func caption(_ remedy: OpenRemedy) -> String? {
        switch remedy {
        case .repair: L10n.VaultOpen.repairCaption
        case .restoreBackup: L10n.VaultOpen.restoreCaption
        case .recoverFromCode: L10n.VaultOpen.recoverCaption
        case .sendDiagnostics: L10n.VaultOpen.diagnosticsCaption
        case .tryAgain, .updateApp: nil
        }
    }
}
