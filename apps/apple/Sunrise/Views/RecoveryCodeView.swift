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
            ProgressView("Setting up recovery…")
                .frame(maxWidth: .infinity, alignment: .center)
        case .show:
            VStack(alignment: .leading, spacing: 14) {
                Text(RecoveryCodeModel.warning)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                wordGrid
                Text("Shown once. Not written to any file, and not in any log.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        case .verify, .mismatch:
            VStack(alignment: .leading, spacing: 14) {
                Text("Type the code back, to prove it is written down somewhere you can read.")
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
                    Label(
                        """
                        That is not the code that was shown. Nothing is wrong \
                        with your account — check what you wrote down, or see \
                        the code again.
                        """,
                        systemImage: "exclamationmark.triangle"
                    )
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
        case .done:
            Label(
                "Recovery is set up. Keep that code where you will find it years from now.",
                systemImage: "checkmark.seal"
            )
            .font(.callout)
        case .notThisDevice:
            Text(
                """
                This \(Platform.deviceName) joined an existing account by pairing, so it \
                does not hold the key a recovery code is made from — and does not need to. \
                The device that created the account is the one that showed the code.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        case let .failed(message):
            VStack(alignment: .leading, spacing: 8) {
                Text("Recovery could not be set up.")
                    .font(.callout.weight(.medium))
                Text(message)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(
                    """
                    Your vault is fine and your tasks are safe on this device. Until this \
                    succeeds, though, this device holds the only copy of your account key: \
                    if you lose it, the data cannot be recovered by anyone, including us.
                    """
                )
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
                Button("I have written it down") { model.beginVerification() }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.written")
            case .verify:
                Button("Show the code again") { model.showAgain() }
                    .accessibilityIdentifier("recovery.again")
                Button("Confirm") { model.confirm(typed) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(typed.trimmed.isEmpty)
                    .accessibilityIdentifier("recovery.confirm")
            case .mismatch:
                Button("Show the code again") {
                    typed = ""
                    model.showAgain()
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier("recovery.again")
                Button("Try again") { model.beginVerification() }
                    .accessibilityIdentifier("recovery.retry")
            case .done, .notThisDevice:
                Button("Done", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.done")
            case .failed:
                Button("Try again") { Task { await model.start() } }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("recovery.retry")
                Button("Not now", action: dismiss)
                    .accessibilityIdentifier("recovery.later")
            }
        }
    }

    private var title: String {
        switch model.phase {
        case .idle, .working: "Setting up recovery"
        case .show: "Your recovery code"
        case .verify, .mismatch: "Type it back"
        case .done: "Recovery is set up"
        case .notThisDevice: "Recovery lives on another device"
        case .failed: "Recovery is not set up"
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
