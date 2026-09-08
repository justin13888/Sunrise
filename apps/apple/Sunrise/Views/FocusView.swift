import SwiftUI

/// Focus: pick something, run a session on it, log what got in the way, and
/// see what finishing it released.
struct FocusView: View {
    @Bindable var model: FocusModel

    #if os(iOS)
    /// Whether this is a phone-width column. `horizontalSizeClass` is an iOS
    /// environment value and has no macOS declaration at all, so it cannot
    /// simply be read and compared on both.
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    #endif

    var body: some View {
        VStack(spacing: 0) {
            controls
            Divider()
            if let session = model.running, let progress = model.progress {
                RunningSessionView(
                    session: session,
                    progress: progress,
                    title: model.runningTitle,
                    interrupt: { await model.logInterruption($0) },
                    end: { await model.end(completingTask: $0) }
                )
                Divider()
            }
            if let cascade = model.cascade {
                CascadeBanner(cascade: cascade) { model.dismissCascade() }
                Divider()
            }
            planList
            if let stats = model.stats {
                Divider()
                FocusStatsStrip(stats: stats)
            }
        }
        .navigationTitle("Focus")
        .task { await model.refresh() }
        .task { await model.follow() }
        .onDisappear { model.stopTicking() }
    }

    /// The two things you choose before a session: how much you have in you,
    /// and how long you want.
    ///
    /// Side by side on a Mac and an iPad, stacked on an iPhone. The two
    /// pickers ask for up to 200 and 260 points, and on a phone the row they
    /// share is 378 — so the energy picker was allotted about a character's
    /// width and "any" came out as "an" over "y". Neither number is a minimum,
    /// so side by side is not a layout a phone can be made to hold; it is one
    /// to stop asking for.
    ///
    /// A size class rather than `ViewThatFits`, which the calendar toolbar
    /// beside this uses and which does not work here. `ViewThatFits` measures
    /// each candidate's *ideal* width, and a menu picker's ideal width is just
    /// its current selection — "any" and "one pomodoro" — so the wide row
    /// reports that it fits and then draws exactly the way this is trying to
    /// stop. The calendar's row is fixed-width segmented controls, whose ideal
    /// widths really do say whether the row fits.
    private var controls: some View {
        layout
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
    }

    @ViewBuilder
    private var layout: some View {
        #if os(iOS)
        if horizontalSizeClass == .compact {
            VStack(alignment: .leading, spacing: 8) {
                energyPicker
                sessionPicker
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            wideControls
        }
        #else
        wideControls
        #endif
    }

    private var wideControls: some View {
        HStack(spacing: 16) {
            energyPicker.frame(maxWidth: 200)
            sessionPicker.frame(maxWidth: 260)
            Spacer()
        }
    }

    private var energyPicker: some View {
        Picker("Energy", selection: $model.energy) {
            Text(energyLabel(energy: nil)).tag(nil as Energy?)
            Text(energyLabel(energy: .low)).tag(Energy.low as Energy?)
            Text(energyLabel(energy: .med)).tag(Energy.med as Energy?)
            Text(energyLabel(energy: .high)).tag(Energy.high as Energy?)
        }
    }

    private var sessionPicker: some View {
        Picker("Session", selection: $model.length) {
            ForEach(SessionLength.offered, id: \.self) { option in
                Text(sessionLengthLabel(length: option)).tag(option)
            }
        }
    }

    @ViewBuilder
    private var planList: some View {
        if let message = model.errorMessage {
            Label(message, systemImage: "exclamationmark.triangle")
                .font(.callout)
                .foregroundStyle(.orange)
                .padding(12)
        }
        if model.plan.isEmpty {
            ContentUnavailableView(
                "Nothing to focus on",
                systemImage: "timer",
                description: Text("Every open task is blocked, done, or in another stream.")
            )
        } else {
            List(model.plan, id: \.task.id) { row in
                PlanRowView(row: row, streamName: model.names.stream(row.task.streamId)) {
                    await model.start(row)
                }
                .disabled(model.isRunning)
            }
            .listStyle(.inset)
        }
    }
}

/// One planner row: what it is, and why it sits where it does.
struct PlanRowView: View {
    let row: PlanRow
    let streamName: String?
    let start: () async -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(row.task.title)
                HStack(spacing: 8) {
                    if let streamName {
                        Text("#\(streamName)").font(.caption).foregroundStyle(.secondary)
                    }
                    // The whole sentence comes from `plan_reason`: leverage,
                    // energy fit and the session that would open, worded once
                    // in the domain so this app and `sunrise-cli` explain the
                    // same ranking the same way.
                    Text(row.reason).font(.caption).foregroundStyle(.secondary)
                    if row.priorSessions > 0 {
                        Text("\(row.priorSessions) prior")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                }
            }
            Spacer(minLength: 0)
            Button("Start") { Task { await start() } }
                .buttonStyle(.borderedProminent)
                .accessibilityLabel("Start a session on “\(row.task.title)”")
        }
        .padding(.vertical, 3)
    }
}

/// The running session: the clock, the interruption buttons, and the two ways
/// to stop.
struct RunningSessionView: View {
    let session: SessionRow
    let progress: SessionProgress
    let title: String?
    let interrupt: (InterruptionReason) async -> Void
    let end: (Bool) async -> Void

    private static let reasons: [InterruptionReason] = [
        .selfInterrupt, .meeting, .blocked, .other
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text(progress.clock)
                    .font(.system(size: 34, weight: .medium, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(progress.overran ? .orange : .primary)
                    .accessibilityLabel("Elapsed \(progress.clock)")

                VStack(alignment: .leading, spacing: 2) {
                    Text(title ?? "Focusing")
                        .font(.headline)
                    Text(remainingLine)
                        .font(.caption)
                        .foregroundStyle(progress.overran ? .orange : .secondary)
                }
                Spacer()
                Button("Done") { Task { await end(true) } }
                    .buttonStyle(.borderedProminent)
                Button("Stop") { Task { await end(false) } }
            }

            HStack(spacing: 8) {
                Text("Interrupted by").font(.caption).foregroundStyle(.secondary)
                ForEach(Self.reasons, id: \.self) { reason in
                    Button(interruptionLabel(reason: reason)) {
                        Task { await interrupt(reason) }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                }
                if !session.interruptions.isEmpty {
                    Text("\(session.interruptions.count) logged")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        }
        .padding(12)
    }

    /// An open-ended session has no remaining time — not zero. Saying "0:00"
    /// there would read as "you are out of time" for a session that never had
    /// a limit.
    private var remainingLine: String {
        if progress.overran { return "over the plan" }
        guard let clock = progress.remainingClock else { return "until done" }
        return "\(clock) left"
    }
}

/// What a completion released. Shown once, dismissible: it is news, not state.
struct CascadeBanner: View {
    let cascade: Cascade
    let dismiss: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.triangle.branch")
            Text(summary)
            Spacer()
            Button("Dismiss", systemImage: "xmark", action: dismiss)
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.green.opacity(0.12))
    }

    private var summary: String {
        let released = cascade.released.count
        let blocked = cascade.stillBlocked.count
        let head = released == 1 ? "Unblocked 1 task" : "Unblocked \(released) tasks"
        return blocked == 0 ? head : "\(head); \(blocked) still waiting on something else"
    }
}

/// Totals and calibration. No score, no streak, no quota — the domain is
/// deliberate about that and so is this strip.
struct FocusStatsStrip: View {
    let stats: FocusTotals

    var body: some View {
        HStack(spacing: 20) {
            stat("Sessions", "\(stats.workSessions)")
            stat("Focused", shortDuration(secs: stats.totalFocusedMs / 1000))
            stat("Interruptions", "\(stats.interruptions)")
            if let overall = stats.overall {
                stat("Estimates run", calibration(overall))
            }
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(value).font(.callout.weight(.medium)).monospacedDigit()
            Text(label).font(.caption2).foregroundStyle(.secondary)
        }
    }

    /// `factor > 1` means work runs long against its estimate — the domain's
    /// reading, stated as it defines it.
    private func calibration(_ row: CalibrationRow) -> String {
        String(format: "%.1f\u{00d7}", row.factor)
    }
}
