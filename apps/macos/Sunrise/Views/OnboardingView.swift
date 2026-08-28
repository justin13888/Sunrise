import SwiftUI

/// First run: there is no vault and no key, so nothing can be lost by making
/// one.
struct OnboardingView: View {
    let create: () async -> Void
    @State private var isWorking = false

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "sunrise")
                .font(.system(size: 48))
                .foregroundStyle(.orange)
            Text("Welcome to Sunrise")
                .font(.largeTitle.weight(.semibold))
            Text(
                """
                Your tasks are encrypted on this Mac with a key only you hold. \
                Sunrise stores it in your Keychain — it never leaves the device, \
                and no server can read your data with or without it.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .frame(maxWidth: 420)

            Button("Create my vault") {
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
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

#Preview {
    OnboardingView(create: {})
}
