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
        case .restored: L10n.Recovery.Restore.titleRestored
        case .incomplete: L10n.Recovery.Restore.titleIncomplete
        default: L10n.Recovery.Restore.title
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
            ProgressView(L10n.Recovery.Restore.signingIn)
                .frame(maxWidth: .infinity, alignment: .center)
        case let .restoring(progress):
            ProgressView(Self.describe(progress))
                .frame(maxWidth: .infinity, alignment: .center)
                .accessibilityIdentifier("restore.progress")
        case .restored:
            aftercare
        case let .incomplete(message):
            VStack(alignment: .leading, spacing: 10) {
                Text(L10n.Recovery.Restore.incomplete)
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
            Text(L10n.Recovery.Restore.entryInstruction)
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
            return L10n.Recovery.Restore.unknownWords(count: model.unknownWords.count, positions: list)
        }
        if let problem = model.checksumProblem {
            return L10n.Recovery.Restore.invalidChecksum(problem: problem)
        }
        return L10n.Recovery.Restore.wordCount(count: model.wordCount)
    }

    private var wordStatusIsProblem: Bool {
        !model.unknownWords.isEmpty || model.checksumProblem != nil
    }

    private var aftercare: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(L10n.Recovery.Restore.aftercare)
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
            if let devices {
                Form { DeviceListSection(model: devices) }
                    .frame(minHeight: 160)
            }
            HStack {
                Button(L10n.Recovery.Restore.rotate) { Task { await model.rotateKeys() } }
                    .accessibilityIdentifier("restore.rotate")
                if let count = model.rotatedStreams {
                    Text(L10n.Recovery.Restore.rotated(count: count))
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
                Button(L10n.Action.cancel, role: .cancel, action: dismiss)
                Spacer()
                Button(L10n.Recovery.Restore.submit) { Task { await model.restoreAccount() } }
                    .buttonStyle(.borderedProminent)
                    .disabled(!model.canRestore)
                    .accessibilityIdentifier("restore.submit")
            case .signingIn, .restoring:
                EmptyView()
            case .restored, .incomplete:
                Spacer()
                Button(L10n.Action.done, action: dismiss)
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("restore.done")
            }
        }
    }

    static func describe(_ progress: RestoreFromCodeModel.Progress) -> String {
        switch progress {
        case .fetching: L10n.Recovery.Restore.fetching
        case .identityOpened: L10n.Recovery.Restore.identityOpened
        case .deviceRegistered: L10n.Recovery.Restore.deviceRegistered
        case let .replaying(applied): L10n.Recovery.Restore.replaying(count: Int(clamping: applied))
        }
    }
}

/// So a restore can drive a `sheet(item:)`. Identity is the object's own, as
/// `PairingModel`'s is: its state changes on every step, and an identity
/// derived from it would rebuild the sheet underneath the user.
extension RestoreFromCodeModel: Identifiable {
    nonisolated var id: ObjectIdentifier { ObjectIdentifier(self) }
}
