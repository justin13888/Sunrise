import Foundation

/// One printed line.
struct PrintRow: Identifiable, Equatable, Sendable {
    let id: String
    /// The narrow left-hand column: a checkbox for a task, a time for a block,
    /// a label for a statistic. Empty is fine and prints as indentation.
    let leading: String
    let title: String
    /// The grey second line — chips, counts, whatever the screen shows under
    /// the title. Empty means the row is one line tall.
    let detail: String

    init(id: String, leading: String = "", title: String, detail: String = "") {
        self.id = id
        self.leading = leading
        self.title = title
        self.detail = detail
    }
}

/// A run of rows under a heading.
struct PrintSection: Identifiable, Equatable, Sendable {
    let heading: String
    let rows: [PrintRow]

    var id: String { heading }
}

/// What a printed page says, with nothing about how it looks.
///
/// The whole point of this type is that "what does ⌘P produce for Today?" is a
/// question a test can ask. `ImageRenderer` and `NSPrintOperation` cannot be
/// driven from a unit test — they want a window server and a print panel — so
/// everything decidable is decided here and the renderer is left with nothing
/// but layout.
struct PrintDocument: Equatable, Sendable {
    let title: String
    /// The line under the title: the date it was printed, plus whatever
    /// narrows the view.
    let subtitle: String
    let sections: [PrintSection]

    /// What the PDF save panel proposes. Derived from the title so two
    /// different lists cannot suggest the same file.
    var suggestedFilename: String {
        let stem = title
            .lowercased()
            .map { $0.isLetter || $0.isNumber ? $0 : "-" }
            .reduce(into: "") { result, character in
                if character == "-", result.last == "-" { return }
                result.append(character)
            }
            .trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        return "\(stem.isEmpty ? "sunrise" : stem).pdf"
    }

    var rowCount: Int { sections.reduce(0) { $0 + $1.rows.count } }

    /// Whether there is anything worth printing.
    ///
    /// Checked before a print panel is raised: a print job that produces one
    /// blank page is worse than a menu item that beeps, because the user only
    /// finds out at the printer.
    var isEmpty: Bool { rowCount == 0 }

    /// How many rows fit on a sheet at the sizes `PrintPageView` draws.
    ///
    /// Counted rather than measured, which is the trade this whole design
    /// makes: pagination becomes a pure function of the document — testable,
    /// deterministic, the same on every Mac — at the cost of leaving some
    /// whitespace at the bottom of a page whose rows all happen to be one line
    /// tall. Measuring instead would put the page break inside `ImageRenderer`,
    /// where nothing can check it.
    static let rowsPerPage = 34

    /// Split into sheets, each carrying the title and the subtitle.
    ///
    /// A section that runs past a page boundary has its heading repeated with
    /// "(continued)", because a column of tasks under no heading at all is the
    /// classic way a printed list stops being readable on page two.
    func paginated(rowsPerPage: Int = PrintDocument.rowsPerPage) -> [PrintDocument] {
        let limit = max(1, rowsPerPage)
        var pages: [PrintDocument] = []
        var current: [PrintSection] = []
        var used = 0

        func flush() {
            guard !current.isEmpty else { return }
            pages.append(PrintDocument(title: title, subtitle: subtitle, sections: current))
            current = []
            used = 0
        }

        for section in sections {
            var remaining = section.rows[...]
            var isContinuation = false
            while !remaining.isEmpty {
                if used >= limit { flush() }
                let take = min(limit - used, remaining.count)
                current.append(
                    PrintSection(
                        heading: isContinuation ? "\(section.heading) (continued)" : section.heading,
                        rows: Array(remaining.prefix(take))
                    )
                )
                used += take
                remaining = remaining.dropFirst(take)
                isContinuation = true
            }
        }
        flush()
        // An empty document still prints one page, which says so. The
        // alternative is a print panel over nothing.
        return pages.isEmpty
            ? [PrintDocument(title: title, subtitle: subtitle, sections: [])]
            : pages
    }
}

/// A date, printed the way a header prints one.
///
/// Not the domain's business: this is a rendering of *this device's* civil
/// date for a piece of paper, which is exactly the kind of thing
/// `docs/07-clients/overview.md` leaves to the client.
enum PrintStamp {
    static func date(msSinceEpoch: UInt64, timeZone: String) -> String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: timeZone) ?? .current
        formatter.dateStyle = .full
        formatter.timeStyle = .none
        return formatter.string(from: Date(timeIntervalSince1970: Double(msSinceEpoch) / 1000))
    }

    static func time(msSinceEpoch: Int64, timeZone: String) -> String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: timeZone) ?? .current
        formatter.dateFormat = "HH:mm"
        return formatter.string(from: Date(timeIntervalSince1970: Double(msSinceEpoch) / 1000))
    }
}

@MainActor
extension PrintDocument {
    /// Any task list — Today, the Inbox, a stream, a context, a search result.
    ///
    /// Sectioned exactly as the screen is, because a printed Today that lost
    /// the Overdue heading would be a different document from the one the user
    /// was looking at when they pressed ⌘P.
    static func taskList(_ model: TaskListModel) -> PrintDocument {
        let sections: [PrintSection] = model.kind.isToday
            ? model.groups.map { group in
                PrintSection(
                    heading: "\(group.section.heading) (\(group.tasks.count))",
                    rows: group.tasks.map { row(model.facets(for: $0)) }
                )
            }
            : [PrintSection(
                heading: "\(model.tasks.count) task\(model.tasks.count == 1 ? "" : "s")",
                rows: model.tasks.map { row(model.facets(for: $0)) }
            )]
        return PrintDocument(
            title: model.kind.title,
            subtitle: PrintStamp.date(msSinceEpoch: model.nowMs, timeZone: model.timeZone),
            sections: sections.filter { !$0.rows.isEmpty }
        )
    }

    /// A day or a week of the calendar, as a list rather than a grid.
    ///
    /// A grid on paper is mostly whitespace, and the thing somebody prints a
    /// day for is the order of it. One section per day, so a week reads as a
    /// week.
    static func calendar(_ model: CalendarModel) -> PrintDocument {
        let sections = (0..<model.span.dayCount).map { offset in
            let placed = model.placed(dayOffset: offset)
            return PrintSection(
                heading: PrintStamp.date(
                    msSinceEpoch: UInt64(max(0, model.dayStartMs(offset: offset))),
                    timeZone: model.timeZone
                ),
                rows: placed.map { block in
                    PrintRow(
                        id: block.id,
                        leading: PrintStamp.time(
                            msSinceEpoch: block.startMs,
                            timeZone: model.timeZone
                        ),
                        title: block.row.title ?? "Untitled block",
                        detail: block.row.taskTitles.joined(separator: ", ")
                    )
                }
            )
        }
        return PrintDocument(
            title: model.span == .day ? "Calendar — Day" : "Calendar — Week",
            subtitle: PrintStamp.date(msSinceEpoch: model.nowMs, timeZone: model.timeZone),
            sections: sections.filter { !$0.rows.isEmpty }
        )
    }

    /// The review, whichever tab is showing.
    ///
    /// Only the two report tabs produce a document. Trends is a chart and
    /// History is a list of links to other documents; printing either would
    /// produce a page of numbers with no chart, which is not what the person
    /// pressing ⌘P asked for. Both already have the CSV/JSON export beside
    /// them, which is the right shape for that data.
    static func review(_ model: ReviewModel) -> PrintDocument {
        let stamp = PrintStamp.date(msSinceEpoch: model.nowMs, timeZone: TimeZone.current.identifier)
        switch model.tab {
        case .weekly:
            guard let report = model.weekly else { return empty("Weekly Review", stamp) }
            return PrintDocument(
                title: "Weekly Review",
                subtitle: stamp,
                sections: weeklySections(report)
            )
        case .daily:
            guard let report = model.daily else { return empty("Daily Review", stamp) }
            return PrintDocument(
                title: "Daily Review",
                subtitle: stamp,
                sections: [
                    tasks("Just captured", report.inbox),
                    tasks("Today", report.today),
                    tasks("Blocked", report.blocked)
                ].filter { !$0.rows.isEmpty }
            )
        case .trends, .history:
            return empty(model.tab.title, stamp)
        }
    }

    /// What ⌘P produces for whatever the window is showing, or `nil` where the
    /// screen has no paper shape at all.
    ///
    /// A function of the destination rather than a `switch` inside the window,
    /// so "does ⌘P do anything on the Focus screen?" is a question with an
    /// answer a test can read. The three that return `nil` are deliberate:
    /// Focus is a single task and a timer, Routines is a set of rules rather
    /// than a set of things to do, and the two briefs are a glance whose whole
    /// value is that they are on screen for sixty seconds.
    static func forDestination(
        _ destination: Destination?,
        list: TaskListModel,
        search: SearchModel,
        calendar: CalendarModel,
        review: ReviewModel
    ) -> PrintDocument? {
        switch destination {
        case .list: taskList(list)
        case .search: taskList(search.results)
        case .calendar: self.calendar(calendar)
        case .review: self.review(review)
        case .focus, .routines, .morning, .evening, .none: nil
        }
    }

    private static func empty(_ title: String, _ subtitle: String) -> PrintDocument {
        PrintDocument(title: title, subtitle: subtitle, sections: [])
    }

    private static func weeklySections(_ report: WeeklyReviewReport) -> [PrintSection] {
        var sections: [PrintSection] = [
            PrintSection(heading: "This week", rows: [
                PrintRow(id: "completed", title: "Completed", detail: "\(report.totals.completed)"),
                PrintRow(id: "deferred", title: "Deferred", detail: "\(report.totals.deferred)"),
                PrintRow(id: "dropped", title: "Dropped", detail: "\(report.totals.dropped)"),
                PrintRow(id: "created", title: "Created", detail: "\(report.totals.created)"),
                PrintRow(id: "reopened", title: "Reopened", detail: "\(report.totals.reopened)")
            ])
        ]
        sections.append(tasks("Inbox to triage", report.inbox))
        sections.append(tasks("Slipped past its date", report.slipped))
        sections.append(
            PrintSection(
                heading: "By stream",
                rows: report.streams.map { stream in
                    PrintRow(
                        id: stream.stream,
                        title: stream.name,
                        detail: "\(stream.completed.count) completed, "
                            + "\(stream.deferred.count) deferred, "
                            + "\(stream.createdUntouched.count) untouched"
                    )
                }
            )
        )
        sections.append(
            PrintSection(
                heading: "Routines drifting",
                rows: report.driftingRoutines.map { drift in
                    PrintRow(
                        id: drift.routine,
                        title: drift.title,
                        detail: "\(drift.missed) of \(drift.expected) missed"
                    )
                }
            )
        )
        return sections.filter { !$0.rows.isEmpty }
    }

    private static func tasks(_ heading: String, _ items: [TaskItem]) -> PrintSection {
        PrintSection(
            heading: "\(heading) (\(items.count))",
            rows: items.map { PrintRow(id: $0.id, leading: "☐", title: $0.title) }
        )
    }

    /// One task row, carrying the same facets the screen shows it with.
    private static func row(_ facets: TaskFacets) -> PrintRow {
        var chips: [String] = []
        if let priority = facets.priority { chips.append("!\(priority)") }
        if let stream = facets.streamName { chips.append("#\(stream)") }
        chips.append(contentsOf: facets.contextNames.map { "@\($0)" })
        if let due = facets.due { chips.append("due \(due.text)") }
        if let scheduled = facets.scheduled, facets.due == nil { chips.append(scheduled.text) }
        if let estimate = facets.estimate { chips.append(estimate) }
        if let energy = facets.energy { chips.append(energy) }
        if facets.isBlocked { chips.append("waiting") }
        return PrintRow(
            id: facets.id,
            leading: facets.isDone ? "☑" : "☐",
            title: facets.title,
            detail: chips.joined(separator: "  ")
        )
    }
}
