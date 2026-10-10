import SwiftUI

/// First run: there is no vault and no key, so nothing can be lost by making
/// one.
///
/// Three branches, not one. "Create my vault" is right for a first Mac and
/// wrong for a second: someone who already has a vault and takes it would end
/// up with two unrelated encrypted stores and no way to merge them. Pairing is
/// how the existing vault reaches this machine, and it has to be offered here,
/// where the choice is actually being made. The recovery code is the way back
/// for someone with no device left to pair with, which is
/// `docs/03-crypto/recovery.md` §Recovery flow step 1 (#349).
struct OnboardingView: View {
    let create: () async -> Void
    /// The session this screen is a phase of. Passed in by ``RootView``, which
    /// owns it; optional only so a preview can stand this screen up without
    /// one, which is also what a preview of the pairing button exercises.
    let session: SessionModel?

    @State private var isWorking = false
    @State private var pairing: PairingModel?
    @State private var settings = AppSettings()
    @State private var tips = KeyboardTips()

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "sunrise")
                .font(.system(size: 48))
                .foregroundStyle(.orange)
            Text(L10n.Onboarding.welcome)
                .font(.largeTitle.weight(.semibold))
            Text(L10n.Onboarding.intro(device: Platform.deviceName))
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)

            Button(L10n.Onboarding.create) {
                isWorking = true
                Task {
                    await create()
                    isWorking = false
                }
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(isWorking)
            .accessibilityIdentifier("onboarding.create")

            if isWorking {
                ProgressView().controlSize(.small)
            }

            Divider().frame(maxWidth: 320)

            VStack(spacing: 6) {
                Text(L10n.Onboarding.alreadyUsing)
                    .font(.callout)
                Button(L10n.Onboarding.pair) { pairing = makePairing() }
                    .controlSize(.large)
                    .disabled(isWorking)
                    .accessibilityIdentifier("onboarding.pair")
                Text(L10n.Onboarding.pairCaption)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 380)
                Button(L10n.Onboarding.restore) { session?.beginRestore() }
                    .disabled(isWorking || session == nil)
                    .accessibilityIdentifier("onboarding.restore")
                Text(L10n.Onboarding.restoreCaption)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 380)
            }

            // `docs/08-features/keyboard.md` §Discoverability. Last on the
            // screen and under a divider: it is an offer, not a step, and a
            // first run must not read as a form to fill in.
            //
            // macOS only. It advertises `?` for the cheat sheet and ⌘⇧N for
            // the global hotkey; a first-run iPhone has neither a keyboard to
            // press the first with nor a hotkey to register the second, so on
            // iOS this would be a coachmark for two things that are not there.
            // The keyboard preferences themselves stay reachable in Settings,
            // which is right for an iPad with a keyboard attached.
            #if os(macOS)
            Divider().frame(maxWidth: 320)
            KeyboardTipsCoachmark(tips: tips)
            #endif
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .sheet(item: $pairing.endingThePairingOnDismiss) { model in
            PairingView(model: model) { pairing = nil }
        }
    }

    /// The relay URL and the process's account bearer, so the pairing runs
    /// over the relay when both are here.
    private func makePairing() -> PairingModel {
        .joining(
            relayURL: settings.relayURL.trimmed,
            account: session?.account,
            adopt: { [session] root, bundle in
                await session?.adoptPairing(root: root, bundle: bundle)
            }
        )
    }
}

/// So a `PairingModel` can drive a `sheet(item:)`.
///
/// Identity is the object's own — one live pairing per sheet — rather than
/// anything derived from its state, which changes on every leg and would
/// rebuild the sheet underneath the user.
extension PairingModel: Identifiable {
    nonisolated var id: ObjectIdentifier { ObjectIdentifier(self) }
}

#Preview {
    OnboardingView(create: {}, session: nil)
}
