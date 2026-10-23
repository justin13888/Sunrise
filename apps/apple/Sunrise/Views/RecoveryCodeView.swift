import SwiftUI

/// The recovery-code ceremony on screen: twenty-four words, once, and a
/// paste-back before the user is allowed to move on.
///
/// Presented as a sheet that does not dismiss by gesture. That is the one
/// interaction decision here worth defending: every other sheet in this app
/// closes on a swipe, and this one must not, because a swipe would be
/// indistinguishable from "done" and the code is shown exactly once. The user
/// leaves by confirming, or — from ``RecoveryCodeModel/Phase/notThisDevice`` and
/// ``RecoveryCodeModel/Phase/failed(_:)`` — by a button that says what it is
/// doing.
///
/// See ``RecoveryCodeModel`` for why the ceremony has the shape it has.
struct RecoveryCodeView: View {
    let model: RecoveryCodeModel
    let dismiss: () -> Void

    @State private var typed = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            header
            content
            Spacer(minLength: 0)
            actions
        }
        .padding(28)
        .frame(minWidth: 420, minHeight: 380)
        .task { await model.start() }
        // A swipe would look exactly like "I have written this down".
        .interactiveDismissDisabled(needsTheUser)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "key.horizontal")
                .font(.title2)
                .foregroundStyle(.orange)
            Text(title)
                .font(.title2.weight(.semibold))
        }
    }

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .idle, .working:
            ProgressView(L10n.Recovery.settingUp)
                .frame(maxWidth: .infinity, alignment: .center)
        case .show:
            VStack(alignment: .leading, spacing: 14) {
                Text(RecoveryCodeModel.warning)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                wordGrid
                Text(L10n.Recovery.shownOnce)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        case .verify, .mismatch:
            VStack(alignment: .leading, spacing: 14) {
                Text(L10n.Recovery.verifyInstruction)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                TextField("abandon ability able …", text: $typed, axis: .vertical)
                    .textFieldStyle(.roundedBorder)
                    .lineLimit(3 ... 6)
                    .font(.body.monospaced())
                    .accessibilityIdentifier("recovery.verify.field")
                    #if os(iOS)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    #endif
                if model.phase == .mismatch {
                    Label(L10n.Recovery.mismatch, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
        case .done:
            Label(L10n.Recovery.done, systemImage: "checkmark.seal")
                .font(.callout)
        case .notThisDevice:
            Text(L10n.Recovery.notThisDevice(device: Platform.deviceName))
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        case let .failed(message):
            VStack(alignment: .leading, spacing: 8) {
                Text(L10n.Recovery.failed)
                    .font(.callout.weight(.medium))
                Text(message)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(L10n.Recovery.failedCaption)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// Six rows of four, matching what `sunrise bootstrap` prints — twenty-four
    /// words on one wrapped line is what a transcription error looks like
    /// before it happens.
    private var wordGrid: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(model.rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 14) {
                    ForEach(row, id: \.self) { word in
                        Text(word)
                            .font(.body.monospaced())
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 8))
        .textSelection(.enabled)
        .accessibilityIdentifier("recovery.words")
    }

    @ViewBuilder
    private var actions: some View {
        HStack {
            Spacer()
            switch model.phase {
            case .idle, .working:
                EmptyView()
            case .show:
                Button(L10n.Recovery.written) { model.beginVerification() }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.written")
            case .verify:
                Button(L10n.Recovery.showAgain) { model.showAgain() }
                    .accessibilityIdentifier("recovery.again")
                Button(L10n.Recovery.confirm) { model.confirm(typed) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(typed.trimmed.isEmpty)
                    .accessibilityIdentifier("recovery.confirm")
            case .mismatch:
                Button(L10n.Recovery.showAgain) {
                    typed = ""
                    model.showAgain()
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier("recovery.again")
                Button(L10n.Action.tryAgain) { model.beginVerification() }
                    .accessibilityIdentifier("recovery.retry")
            case .done, .notThisDevice:
                Button(L10n.Action.done, action: dismiss)
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.done")
            case .failed:
                Button(L10n.Action.tryAgain) { Task { await model.start() } }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.retry")
                Button(L10n.Recovery.notNow, action: dismiss)
                    .accessibilityIdentifier("recovery.later")
            }
        }
    }

    private var title: String {
        switch model.phase {
        case .idle, .working: L10n.Recovery.titleSettingUp
        case .show: L10n.Recovery.titleShow
        case .verify, .mismatch: L10n.Recovery.titleVerify
        case .done: L10n.Recovery.titleDone
        case .notThisDevice: L10n.Recovery.titleNotThisDevice
        case .failed: L10n.Recovery.titleFailed
        }
    }

    /// Whether closing the sheet right now would lose something.
    private var needsTheUser: Bool {
        switch model.phase {
        case .show, .verify, .mismatch: true
        case .idle, .working, .done, .notThisDevice, .failed: false
        }
    }
}

#Preview("Shown") {
    RecoveryCodeView(
        model: RecoveryCodeModel(publish: {
            (0 ..< 24).map { "word\($0)" }.joined(separator: " ")
        }),
        dismiss: {}
    )
}

#Preview("Paired device") {
    RecoveryCodeView(model: RecoveryCodeModel(publish: { nil }), dismiss: {})
}

/// So a `RecoveryCodeModel` can drive a `sheet(item:)`.
///
/// Identity is the object's own — one ceremony per sheet — rather than
/// anything derived from its phase, which changes at every step and would
/// rebuild the sheet underneath the user mid-ceremony. The same rule
/// `PairingModel` follows, for the same reason.
extension RecoveryCodeModel: Identifiable {
    nonisolated var id: ObjectIdentifier { ObjectIdentifier(self) }
}
