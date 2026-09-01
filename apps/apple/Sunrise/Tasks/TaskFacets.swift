import Foundation

/// A day, as the domain words it.
struct DayLabel: Equatable {
    /// `yesterday` / `today` / `tomorrow` / `+3d` / `-3d`.
    let text: String
    /// Whether it is behind now. Not derivable from `text` without parsing it,
    /// which is why the seam returns both.
    let isPast: Bool

    init(_ day: RelativeDay) {
        text = day.text
        isPast = day.isPast
    }
}

/// Everything one task row shows.
///
/// Built entirely out of the exported vocabulary — `relativeDay`,
/// `shortDuration`, `energyLabel`, `constraintSummary`. Nothing here decides
/// what a fact *means*; it decides only which facts a row has room for.
struct TaskFacets: Equatable {
    let id: EntityRef
    let title: String
    let isDone: Bool
    /// 1 is highest.
    let priority: UInt8?
    /// "low" / "med" / "high"; absent when the task carries no facet, because
    /// "any" is a planner input rather than something to show on a row.
    let energy: String?
    /// `30m`, `1h30`.
    let estimate: String?
    let scheduled: DayLabel?
    let due: DayLabel?
    let streamName: String?
    let contextNames: [String]
    /// `2 constraints (1 hard)`, absent when there are none.
    let constraints: String?
    /// Waiting on another task.
    let isBlocked: Bool
    /// How many times this has been pushed out. Shown from the second
    /// deferral, where it stops being noise and starts being a pattern.
    let deferrals: Int64

    init(task: TaskItem, nowMs: UInt64, timeZone: String, names: NameBook) {
        id = task.id
        title = task.title
        isDone = task.state == .done || task.state == .cancelled
        priority = task.priority
        energy = task.energy.map { energyLabel(energy: $0) }
        estimate = task.estimatedDurationS.map { shortDuration(secs: $0) }
        scheduled = task.scheduledAt.map {
            DayLabel(relativeDay(at: $0, nowMs: nowMs, tz: timeZone))
        }
        due = task.dueAt.map {
            DayLabel(relativeDay(at: $0, nowMs: nowMs, tz: timeZone))
        }
        streamName = names.stream(task.streamId)
        contextNames = names.contextNames(task.contexts)
        let summary = task.schedulingConstraints.isEmpty
            ? ""
            : constraintSummary(constraints: task.schedulingConstraints)
        constraints = summary.isEmpty ? nil : summary
        isBlocked = !task.blockedBy.isEmpty
        deferrals = task.deferredCount
    }
}

/// Today's rows, grouped the way `docs/08-features/planning-views.md` composes
/// the view.
struct TodayGroup: Equatable, Identifiable {
    let section: TodaySection
    let tasks: [TaskItem]

    var id: Int { section.rank }
}

extension TodaySection {
    /// The spec's order. `Overdue` is last and folded; it is a backlog, not the
    /// first thing to read in the morning.
    var rank: Int {
        switch self {
        case .scheduled: 0
        case .due: 1
        case .overdue: 2
        }
    }

    var heading: String {
        switch self {
        case .scheduled: "Scheduled"
        case .due: "Due today"
        case .overdue: "Overdue"
        }
    }

    /// Folded by default, per the spec.
    var startsFolded: Bool { self == .overdue }
}

enum TaskGrouping {
    /// Partition Today's rows into sections, **preserving the core's order**
    /// inside each one.
    ///
    /// The core already sorted by `COALESCE(scheduled_at, due_at)`; re-sorting
    /// here would silently replace its ordering with this file's opinion.
    /// Empty sections are dropped rather than shown as empty headings.
    static func today(
        tasks: [TaskItem],
        nowMs: UInt64,
        timeZone: String
    ) -> [TodayGroup] {
        var buckets: [Int: [TaskItem]] = [:]
        var sections: [Int: TodaySection] = [:]
        for task in tasks {
            let section = todaySection(
                scheduledAt: task.scheduledAt,
                dueAt: task.dueAt,
                nowMs: nowMs,
                tz: timeZone
            )
            buckets[section.rank, default: []].append(task)
            sections[section.rank] = section
        }
        return buckets.keys.sorted().compactMap { rank in
            guard let section = sections[rank], let tasks = buckets[rank] else { return nil }
            return TodayGroup(section: section, tasks: tasks)
        }
    }
}
