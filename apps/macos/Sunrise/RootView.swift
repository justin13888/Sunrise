import SwiftUI

/// What the window shows.
///
/// A placeholder until the vault state machine lands: this build proves the
/// shell compiles, links `SunriseCore.xcframework`, and can call across the
/// UniFFI seam.
struct RootView: View {
    var body: some View {
        VStack(spacing: 8) {
            Text("Sunrise")
                .font(.largeTitle)
            Text("Core linked · \(shortDuration(secs: 5400))")
                .font(.callout)
                .foregroundStyle(.secondary)
                .monospacedDigit()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

#Preview {
    RootView()
}
