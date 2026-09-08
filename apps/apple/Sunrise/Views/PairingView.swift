import SwiftUI

/// The pairing sheet, from either side.
///
/// One view for both roles on purpose. The two devices walk the *same* six
/// legs in the same order — they simply alternate who is showing and who is
/// pasting — and two screens would be two places for that order to drift out
/// of step with `sunrise-pairing`.
struct PairingView: View {
    @Bindable var model: PairingModel
    let dismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                content
                    .padding(24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            Divider()
            footer
        }
        .frame(width: 560, height: 520)
    }

    // MARK: - Chrome

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(model.intent == .addThisMac ? "Pair this Mac" : "Add a device")
                .font(.headline)
            Spacer()
            if let progress = model.progress {
                Text("Step \(progress.leg) of \(progress.of)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("pairing.progress")
            }
            // The seam's own view of where the handshake is, rather than this
            // screen's. They can only disagree if this screen has a bug, which
            // is exactly why it is worth showing.
            if let step = model.handshakeStep {
                Text(Self.label(for: step))
                    .font(.caption.monospaced())
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(.quaternary, in: Capsule())
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    private var footer: some View {
        HStack {
            if case .done = model.phase {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("pairing.done")
            } else {
                Button("Cancel", role: .cancel) {
                    model.cancel()
                    dismiss()
                }
                .accessibilityIdentifier("pairing.cancel")
                Spacer()
                primaryAction
            }
        }
        .controlSize(.large)
        .padding(16)
    }

    @ViewBuilder
    private var primaryAction: some View {
        switch model.phase {
        case .idle:
            Button("Show my pairing code") { model.begin() }
                .buttonStyle(.borderedProminent)
                .disabled(model.accountEmail.trimmed.isEmpty)
                .accessibilityIdentifier("pairing.begin")
        case let .handOff(handOff):
            Button(handOff.leg == .root ? "I've pasted it" : "Continue") { model.advance() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("pairing.continue")
        case .awaiting:
            Button("Continue") { Task { await model.submit() } }
                .buttonStyle(.borderedProminent)
                .disabled(model.pasted.trimmed.isEmpty)
                .accessibilityIdentifier("pairing.submit")
        case .comparing, .working, .done:
            EmptyView()
        case .mismatch, .failed:
            Button("Start over") { model.cancel() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("pairing.restart")
        }
    }

    // MARK: - The legs

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .idle:
            accountForm
        case let .handOff(handOff):
            handOffLeg(handOff)
        case let .awaiting(prompt):
            awaitingLeg(prompt)
        case let .comparing(sas):
            SASConfirmation(
                sas: sas,
                isNewDevice: model.intent == .addThisMac,
                answer: { matched in Task { await model.confirm(matched: matched) } }
            )
        case .working:
            Label(
                model.intent == .addThisMac
                    ? "Opening your vault on this Mac…"
                    : "Sealing your vault key for the other device…",
                systemImage: "hourglass"
            )
        case let .done(summary):
            outcome(
                symbol: "checkmark.seal",
                tint: .green,
                title: "Paired",
                detail: summary
            )
        case .mismatch:
            outcome(
                symbol: "exclamationmark.shield",
                tint: .red,
                title: "The digits did not match",
                detail: """
                    Sunrise has thrown this pairing away and no key was sent. \
                    Two devices talking directly to each other always show the \
                    same six digits, so a mismatch means something was relaying \
                    between them. Start again, and if it happens twice, do it \
                    on a network you trust.
                    """
            )
        case let .failed(message):
            outcome(
                symbol: "exclamationmark.triangle",
                tint: .orange,
                title: "Pairing stopped",
                detail: message
            )
        }
    }

    /// The one thing this Mac has to be told before it can publish a code: the
    /// account. It is hashed to four bytes before it reaches the payload, so
    /// what travels names the account without naming the person.
    private var accountForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Which account is this?")
                .font(.title3.weight(.semibold))
            Text(
                """
                Sunrise puts a four-byte hash of this address in the pairing \
                code so the other Mac can tell it is being asked about the \
                right account. The address itself does not travel.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            TextField("Email", text: $model.accountEmail, prompt: Text("you@example.com"))
                .textInput(.email)
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier("pairing.email")
            if !model.accountTag.isEmpty {
                LabeledContent("Account tag", value: model.accountTag)
                    .monospaced()
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func handOffLeg(_ handOff: PairingModel.HandOff) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(handOff.title).font(.title3.weight(.semibold))
            Text(handOff.instruction)
                .font(.callout)
                .foregroundStyle(.secondary)

            if handOff.drawsCode {
                if let code = QRCode.image(for: handOff.text) {
                    Image(platformImage: code)
                        .interpolation(.none)
                        .frame(width: code.size.width, height: code.size.height)
                        .padding(12)
                        .background(.white, in: RoundedRectangle(cornerRadius: 8))
                        .frame(maxWidth: .infinity, alignment: .center)
                        .accessibilityIdentifier("pairing.qr")
                        .accessibilityLabel("Pairing QR code")
                } else {
                    // Saying so, rather than showing a blank square: the text
                    // below is a complete substitute and the user needs to know
                    // it is the one to use.
                    Label(
                        "This Mac could not draw the code. Copy the text instead.",
                        systemImage: "exclamationmark.triangle"
                    )
                    .foregroundStyle(.orange)
                }
                if !model.accountTag.isEmpty {
                    LabeledContent("Account tag", value: model.accountTag)
                        .monospaced()
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            CopyableBlock(text: handOff.text, identifier: "pairing.handoff")
        }
    }

    private func awaitingLeg(_ prompt: PairingModel.Prompt) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(prompt.title).font(.title3.weight(.semibold))
            Text(prompt.instruction)
                .font(.callout)
                .foregroundStyle(.secondary)
            // A text field, not a camera. `docs/07-clients/parity-matrix.md`
            // says "camera or paste", and a paste field needs no entitlement,
            // works on a Mac with the lid shut, and is the only thing that can
            // carry the Noise messages anyway — they are far too long to scan.
            TextEditor(text: $model.pasted)
                // A Noise message in base64. Autocapitalising its first
                // character or "correcting" a run of letters inside it
                // produces a block that looks right and no longer decodes,
                // which is the worst failure this screen can have: the
                // handshake is refused and nothing on screen says why.
                .textInput(.opaque)
                .font(.system(.body, design: .monospaced))
                .frame(height: 120)
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(.quaternary))
                .accessibilityIdentifier("pairing.paste")
            HStack {
                Button("Paste", systemImage: "doc.on.clipboard") {
                    model.pasted = PlatformPasteboard.string ?? model.pasted
                }
                Spacer()
            }
        }
    }

    private func outcome(
        symbol: String,
        tint: Color,
        title: String,
        detail: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Image(systemName: symbol)
                .font(.system(size: 40))
                .foregroundStyle(tint)
            Text(title).font(.title3.weight(.semibold))
            Text(detail)
                .font(.callout)
                .foregroundStyle(.secondary)
        }
    }

    private static func label(for step: PairingStep) -> String {
        switch step {
        case .handshaking: "handshaking"
        case .awaitingConfirmation: "awaiting SAS"
        case .confirmed: "confirmed"
        case .finished: "finished"
        }
    }
}

/// The SAS screen.
///
/// This is the whole of the authentication on the numeric path, and it is
/// deliberately the least automatic screen in the app. Six digits is about 20
/// bits — safe only because a machine in the middle gets exactly one online
/// attempt and a human aborts it. So: no auto-advance, no default button, no
/// pre-selected answer. The user has to read the digits off the other device
/// and say, in as many words, that they are the same.
private struct SASConfirmation: View {
    let sas: String
    /// Only to word the instruction. Both sides do exactly the same thing.
    let isNewDevice: Bool
    let answer: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Do both Macs show these digits?")
                .font(.title3.weight(.semibold))

            Text(spaced)
                .font(.system(size: 44, weight: .semibold, design: .monospaced))
                .tracking(4)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.vertical, 8)
                .accessibilityIdentifier("pairing.sas")
                .accessibilityLabel("Pairing digits \(sas.map(String.init).joined(separator: " "))")

            Text(
                """
                Read them aloud, or look at the other screen. Both Macs must be \
                showing the same six digits, and both of you have to confirm \
                before anything is sent.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)

            Text(
                isNewDevice
                    ? """
                        These digits are derived from every message the two Macs \
                        have exchanged. If anything were sitting between them, it \
                        could not make both screens agree — it would have to guess \
                        six digits, once, with you watching.
                        """
                    : """
                        Nothing has left this Mac yet. Your vault key is sealed and \
                        sent only after you confirm below, and only for the device \
                        on the other end of these digits.
                        """
            )
            .font(.footnote)
            .foregroundStyle(.secondary)

            HStack(spacing: 12) {
                // No `.defaultAction`: a return keypress must not be able to
                // confirm a code nobody compared.
                Button("The digits match") { answer(true) }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("pairing.sas.match")
                Button("They're different", role: .destructive) { answer(false) }
                    .accessibilityIdentifier("pairing.sas.mismatch")
            }
            .controlSize(.large)
        }
    }

    /// Grouped three and three. A run of six digits is read wrong far more
    /// often than two groups of three, and being read wrong is the failure
    /// this screen exists to prevent.
    private var spaced: String {
        guard sas.count == 6 else { return sas }
        let middle = sas.index(sas.startIndex, offsetBy: 3)
        return "\(sas[sas.startIndex..<middle]) \(sas[middle...])"
    }
}

/// A block of protocol text with the one button that matters next to it.
private struct CopyableBlock: View {
    let text: String
    let identifier: String
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ScrollView {
                Text(text)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .frame(height: 110)
            .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 6))
            .accessibilityIdentifier(identifier)

            HStack(spacing: 8) {
                Button(copied ? "Copied" : "Copy", systemImage: "doc.on.doc") {
                    PlatformPasteboard.set(text)
                    copied = true
                }
                .accessibilityIdentifier("\(identifier).copy")
                if copied {
                    Image(systemName: "checkmark").foregroundStyle(.green)
                }
            }
        }
        .onChange(of: text) { copied = false }
    }
}

#Preview("SAS") {
    PairingView(model: PairingModel(intent: .addThisMac), dismiss: {})
}
