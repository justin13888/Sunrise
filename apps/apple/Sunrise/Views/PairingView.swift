import SwiftUI

/// The pairing sheet, from either side, over either transport.
///
/// One view for both roles on purpose. The two devices walk the *same* legs in
/// the same order — they simply alternate who is showing and who is pasting —
/// and two screens would be two places for that order to drift out of step
/// with `sunrise-pairing`. Over the relay the legs collapse to three (scan,
/// compare, done) and the same cases draw them: the code is a hand-off, the
/// digits are the SAS screen, and everything in between is `.working`.
///
/// It was six until the account's signing key stopped travelling (#105). The
/// device holding the vault cannot certify keys the joining device has not
/// minted, so the single final hand-over became a round trip; the alternation
/// that made one view serve both roles is exactly what made that a three-line
/// change here.
struct PairingView: View {
    @Bindable var model: PairingModel
    let dismiss: () -> Void
    /// The scanner sheet, over this one. Only the code leg offers it.
    @State private var scanning = false

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
        .macSheetFrame(width: 560, height: 520)
        .sheet(isPresented: $scanning) {
            QRScannerView(
                source: .current,
                found: { payload in
                    scanning = false
                    model.pasted = payload
                    Task { await model.submit() }
                },
                pasteInstead: { scanning = false }
            )
        }
    }

    // MARK: - Chrome

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(
                model.intent == .addThisMac
                    ? L10n.Pairing.titleJoin(device: Platform.deviceName)
                    : L10n.Pairing.titleAdd
            )
                .font(.headline)
            Spacer()
            if let progress = model.progress {
                Text(L10n.Pairing.progress(step: progress.leg, total: progress.of))
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
                Button(L10n.Action.done) { dismiss() }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("pairing.done")
            } else {
                Button(L10n.Action.cancel, role: .cancel) {
                    model.cancel()
                    dismiss()
                }
                .accessibilityIdentifier("pairing.cancel")
                if model.offersManualFallback {
                    Button(L10n.Pairing.manualInstead) { model.useManualInstead() }
                        .accessibilityIdentifier("pairing.useManual")
                }
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
            Button(L10n.Pairing.begin) { Task { await model.begin() } }
                .buttonStyle(.borderedProminent)
                .disabled(model.accountEmail.trimmed.isEmpty)
                .accessibilityIdentifier("pairing.begin")
        case .handOff where model.transport == .relay:
            // Nothing to press: the relay tells this device when the code has
            // been read, and the digits replace it.
            ProgressView().controlSize(.small)
        case let .handOff(handOff):
            Button(handOff.leg == .grant ? L10n.Pairing.pastedIt : L10n.Pairing.continueButton) {
                model.advance()
            }
            .buttonStyle(.borderedProminent)
            .accessibilityIdentifier("pairing.continue")
        case .awaiting:
            Button(L10n.Pairing.continueButton) { Task { await model.submit() } }
                .buttonStyle(.borderedProminent)
                .disabled(model.pasted.trimmed.isEmpty)
                .accessibilityIdentifier("pairing.submit")
        case .comparing, .working, .done:
            EmptyView()
        case .mismatch, .failed:
            Button(L10n.Pairing.startOver) { model.cancel() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("pairing.restart")
        }
    }

    // MARK: - The legs

    @ViewBuilder
    private var content: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let notice = model.notice, !model.phase.isOutcome {
                Label(notice, systemImage: "arrow.left.arrow.right")
                    .font(.callout)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.orange.opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
                    .accessibilityIdentifier("pairing.notice")
            }
            phaseContent
        }
    }

    @ViewBuilder
    private var phaseContent: some View {
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
        case let .working(label):
            Label(label, systemImage: "hourglass")
                .accessibilityIdentifier("pairing.working")
        case let .done(summary):
            outcome(
                symbol: "checkmark.seal",
                tint: .green,
                title: L10n.Pairing.paired,
                detail: summary
            )
        case .mismatch:
            outcome(
                symbol: "exclamationmark.shield",
                tint: .red,
                title: L10n.Pairing.mismatchTitle,
                detail: L10n.Pairing.mismatchDetail
            )
        case let .failed(message):
            outcome(
                symbol: "exclamationmark.triangle",
                tint: .orange,
                title: L10n.Pairing.failedTitle,
                detail: message
            )
        }
    }

    /// The one thing this Mac has to be told before it can publish a code: the
    /// account. It is hashed to four bytes before it reaches the payload, so
    /// what travels names the account without naming the person.
    private var accountForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(L10n.Pairing.accountQuestion)
                .font(.title3.weight(.semibold))
            Text(L10n.Pairing.accountExplanation)
                .font(.callout)
                .foregroundStyle(.secondary)
            TextField(
                L10n.Pairing.email,
                text: $model.accountEmail,
                prompt: Text(verbatim: "you@example.com")
            )
            .textInput(.email)
            .textFieldStyle(.roundedBorder)
            .accessibilityIdentifier("pairing.email")
            if !model.accountTag.isEmpty {
                LabeledContent(L10n.Pairing.accountTag, value: model.accountTag)
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
                        .accessibilityLabel(L10n.Pairing.qrLabel)
                } else {
                    // Saying so, rather than showing a blank square: the text
                    // below is a complete substitute and the user needs to know
                    // it is the one to use.
                    Label(
                        L10n.Pairing.qrFailed(device: Platform.deviceName),
                        systemImage: "exclamationmark.triangle"
                    )
                    .foregroundStyle(.orange)
                }
                if !model.accountTag.isEmpty {
                    LabeledContent(L10n.Pairing.accountTag, value: model.accountTag)
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
            // Camera or paste, as `docs/07-clients/parity-matrix.md` says, and
            // only on the code leg: the code is the one message small enough
            // to be a symbol. The paste field stays under the button because
            // it needs no permission, works on a Mac with the lid shut, and is
            // the only thing that can carry the Noise messages of the manual
            // flow — they are far too long to scan.
            if prompt.leg == .code {
                Button(L10n.Pairing.scanCode, systemImage: "qrcode.viewfinder") { scanning = true }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .accessibilityIdentifier("pairing.scan")
            }
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
                Button(L10n.Pairing.paste, systemImage: "doc.on.clipboard") {
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
        case .handshaking: L10n.Pairing.stepHandshaking
        case .awaitingConfirmation: L10n.Pairing.stepAwaitingConfirmation
        case .confirmed: L10n.Pairing.stepConfirmed
        case .finished: L10n.Pairing.stepFinished
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
            Text(L10n.Pairing.sasTitle)
                .font(.title3.weight(.semibold))

            Text(spaced)
                .font(.system(size: 44, weight: .semibold, design: .monospaced))
                .tracking(4)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.vertical, 8)
                .accessibilityIdentifier("pairing.sas")
                .accessibilityLabel(
                    L10n.Pairing.sasLabel(digits: sas.map(String.init).joined(separator: " "))
                )

            Text(L10n.Pairing.sasInstruction)
                .font(.callout)
                .foregroundStyle(.secondary)

            Text(
                isNewDevice
                    ? L10n.Pairing.sasJoining
                    : L10n.Pairing.sasSponsoring(device: Platform.deviceName)
            )
            .font(.footnote)
            .foregroundStyle(.secondary)

            HStack(spacing: 12) {
                // No `.defaultAction`: a return keypress must not be able to
                // confirm a code nobody compared.
                Button(L10n.Pairing.sasMatch) { answer(true) }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("pairing.sas.match")
                Button(L10n.Pairing.sasDiffer, role: .destructive) { answer(false) }
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
                Button(copied ? L10n.Pairing.copied : L10n.Pairing.copy, systemImage: "doc.on.doc") {
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

extension Binding where Value == PairingModel? {
    /// The binding a pairing sheet presents from, which ends the pairing
    /// however the sheet goes away.
    ///
    /// SwiftUI clears the binding itself on an iOS swipe-down, so this setter
    /// is the one place every dismissal passes through — Cancel and Done
    /// included, for which ``PairingModel/dismissed()`` is a no-op or a repeat.
    var endingThePairingOnDismiss: Binding<PairingModel?> {
        Binding(
            get: { wrappedValue },
            set: { next in
                // SwiftUI writes a presentation binding on the main thread.
                MainActor.assumeIsolated {
                    if next == nil { wrappedValue?.dismissed() }
                }
                wrappedValue = next
            }
        )
    }
}

#Preview("SAS") {
    PairingView(model: PairingModel(intent: .addThisMac), dismiss: {})
}
