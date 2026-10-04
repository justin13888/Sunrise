import SwiftUI

/// Restoring an account from its recovery code, on screen (#349).
///
/// One sheet carries the whole walk: the twenty-four words, the browser
/// sign-in, the restore's progress, and the aftercare `docs/03-crypto/
/// recovery.md` §Recovery flow step 7 asks for. Both aftercare actions are on
/// the same screen as the news that the account is back, because a user who
/// has just lost every device is the user most likely to close the sheet and
/// never come back for them.
///
/// See ``RestoreFromCodeModel`` for the sequence.
struct RestoreFromCodeView: View {
    @Bindable var model: RestoreFromCodeModel
    /// The open vault, once the restore has opened it. The aftercare's device
    /// list reads it.
    let bridge: CoreBridge?
    let dismiss: () -> Void

    @State private var devices: DeviceListModel?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "key.horizontal")
                    .font(.title2)
                    .foregroundStyle(.orange)
                Text(title)
                    .font(.title2.weight(.semibold))
            }
            content
            Spacer(minLength: 0)
            actions
        }
        .padding(28)
        .frame(minWidth: 440, minHeight: 420)
        // A restore in flight cannot be called back, so the sheet cannot be
        // swiped away from under it either.
        .interactiveDismissDisabled(isBusy)
    }

    private var title: String {
        switch model.phase {
        case .restored: "Your account is back"
        case .incomplete: "Your account is back, still catching up"
        default: "Restore from your recovery code"
        }
    }

    private var isBusy: Bool {
        switch model.phase {
        case .signingIn, .restoring: true
        default: false
        }
    }

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .entering, .failed:
            entry
        case .signingIn:
            ProgressView("Sign in again in your browser…")
                .frame(maxWidth: .infinity, alignment: .center)
        case let .restoring(progress):
            ProgressView(Self.describe(progress))
                .frame(maxWidth: .infinity, alignment: .center)
                .accessibilityIdentifier("restore.progress")
        case .restored:
            aftercare
        case let .incomplete(message):
            VStack(alignment: .leading, spacing: 10) {
                Text(
                    """
                    Your vault is restored and open. Part of its history had \
                    not arrived yet; it finishes the next time Sunrise syncs.
                    """
                )
                .font(.callout)
                Text(message)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var entry: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(
                """
                Type the twenty-four words you saved when you set Sunrise up. \
                Your provider will ask you to sign in again before the relay \
                releases your encrypted account.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            TextField("abandon ability able …", text: $model.text, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(4 ... 8)
                .font(.body.monospaced())
                .accessibilityIdentifier("restore.words")
                #if os(iOS)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                #endif
            Text(wordStatus)
                .font(.caption)
                .foregroundStyle(wordStatusIsProblem ? Color.red : Color.secondary)
                .accessibilityIdentifier("restore.status")
            if case let .failed(failure) = model.phase {
                Label(failure.message, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("restore.error")
            }
        }
    }

    /// Positions, never words: what sits beside a recovery code ends up in
    /// screenshots.
    private var wordStatus: String {
        if !model.unknownWords.isEmpty {
            let list = model.unknownWords.map(String.init).joined(separator: ", ")
            return model.unknownWords.count == 1
                ? "Word \(list) is not a recovery-code word."
                : "Words \(list) are not recovery-code words."
        }
        if let problem = model.checksumProblem {
            return "All twenty-four are real words, but they are not a valid code: \(problem)"
        }
        return "\(model.wordCount) of 24 words"
    }

    private var wordStatusIsProblem: Bool {
        !model.unknownWords.isEmpty || model.checksumProblem != nil
    }

    private var aftercare: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(
                """
                Two things are worth doing now, and neither is automatic. \
                Remove the devices you lost: until you do, anything still \
                holding them can read what this account writes. Then rotate \
                your Stream keys, which bounds what a lost device keeps reading.
                """
            )
            .font(.callout)
            .fixedSize(horizontal: false, vertical: true)
            if let devices {
                Form { DeviceListSection(model: devices) }
                    .frame(minHeight: 160)
            }
            HStack {
                Button("Rotate Stream keys") { Task { await model.rotateKeys() } }
                    .accessibilityIdentifier("restore.rotate")
                if let count = model.rotatedStreams {
                    Text(count == 1 ? "1 Stream key rotated." : "\(count) Stream keys rotated.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let problem = model.rotationProblem {
                    Text(problem)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .task(id: bridge.map(ObjectIdentifier.init)) {
            devices = bridge.map { DeviceListModel(bridge: $0) }
        }
    }

    @ViewBuilder
    private var actions: some View {
        HStack {
            switch model.phase {
            case .entering, .failed:
                Button("Cancel", role: .cancel, action: dismiss)
                Spacer()
                Button("Sign in and restore") { Task { await model.restoreAccount() } }
                    .buttonStyle(.borderedProminent)
                    .disabled(!model.canRestore)
                    .accessibilityIdentifier("restore.submit")
            case .signingIn, .restoring:
                EmptyView()
            case .restored, .incomplete:
                Spacer()
                Button("Done", action: dismiss)
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("restore.done")
            }
        }
    }

    static func describe(_ progress: RestoreFromCodeModel.Progress) -> String {
        switch progress {
        case .fetching: "Fetching your encrypted account…"
        case .identityOpened: "Code accepted. Setting up this device…"
        case .deviceRegistered: "Reading your history…"
        case let .replaying(applied): "Reading your history: \(applied) changes so far…"
        }
    }
}

/// So a restore can drive a `sheet(item:)`. Identity is the object's own, as
/// `PairingModel`'s is: its state changes on every step, and an identity
/// derived from it would rebuild the sheet underneath the user.
extension RestoreFromCodeModel: Identifiable {
    nonisolated var id: ObjectIdentifier { ObjectIdentifier(self) }
}
