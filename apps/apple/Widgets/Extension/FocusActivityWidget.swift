#if os(iOS)
import ActivityKit
import SwiftUI
import WidgetKit

/// The focus session's Live Activity: the timer and the task, in the Dynamic
/// Island and on the Lock Screen.
///
/// `docs/07-clients/mobile-ios.md` §Live Activities. The app starts and ends
/// it (`iOS/FocusLiveActivity.swift`); this only draws ``FocusActivityState``,
/// which carries the title and two instants, so there is nothing here to
/// decide and nothing that reads the vault.
struct FocusActivityWidget: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: FocusActivityAttributes.self) { context in
            FocusActivityLockScreen(state: context.state)
                .activityBackgroundTint(nil)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    Label("Focus", systemImage: "timer")
                        .labelStyle(.iconOnly)
                        .foregroundStyle(.orange)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    FocusActivityClock(state: context.state)
                        .font(.title2.monospacedDigit())
                        .multilineTextAlignment(.trailing)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    Text(context.state.title)
                        .font(.headline)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            } compactLeading: {
                Image(systemName: "timer")
                    .foregroundStyle(.orange)
                    .accessibilityLabel("Focus")
            } compactTrailing: {
                FocusActivityClock(state: context.state)
                    .monospacedDigit()
                    .frame(maxWidth: 56)
                    .multilineTextAlignment(.trailing)
            } minimal: {
                Image(systemName: "timer")
                    .foregroundStyle(.orange)
                    .accessibilityLabel("Focus")
            }
        }
    }
}

/// The Lock Screen banner.
struct FocusActivityLockScreen: View {
    let state: FocusActivityState

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: "timer")
                .font(.title2)
                .foregroundStyle(.orange)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(state.endsAt == nil ? "Focusing" : "Focusing · time left")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(state.title)
                    .font(.headline)
                    .lineLimit(2)
            }
            Spacer(minLength: 8)
            FocusActivityClock(state: state)
                .font(.title.monospacedDigit())
                .multilineTextAlignment(.trailing)
        }
        .padding()
        .accessibilityElement(children: .combine)
    }
}

/// The timer, drawn by the system so it keeps time with Sunrise suspended.
///
/// Counts down to the end of a sized session, and stops at zero rather than
/// going negative — an overrun is the Focus screen's to show. A session with no
/// planned end counts up from its start.
struct FocusActivityClock: View {
    let state: FocusActivityState

    var body: some View {
        if let endsAt = state.endsAt, endsAt > state.startedAt {
            Text(timerInterval: state.startedAt...endsAt, countsDown: true)
        } else {
            Text(state.startedAt, style: .timer)
        }
    }
}
#endif
