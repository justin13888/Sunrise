import Foundation
import Testing

@testable import Sunrise

/// What ⌘P and "Export as PDF…" would produce.
///
/// Everything decidable about a printed page lives in `PrintDocument`, and
/// this is why: `ImageRenderer` and `NSPrintOperation` want a window server and
/// a print panel, so a design that decided what to print inside the renderer
/// would be a feature with no test at all.
@MainActor
struct PrintDocumentTests {
    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    @Test
    func aTaskListPrintsItsRowsUnderItsOwnTitle() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        _ = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        _ = try await vault.bridge.submit(.createTask(draft: draft("Book the van")))
        await list.refresh()

        let document = PrintDocument.taskList(list)

        #expect(document.title == "Inbox")
        #expect(!document.subtitle.isEmpty, "a printed page has to say when it was printed")
        #expect(document.rowCount == 2)
        #expect(!document.isEmpty)
        let titles = document.sections.flatMap(\.rows).map(\.title).sorted()
        #expect(titles == ["Book the van", "Renew passport"])
        // Open tasks print an empty box, which is the point of printing one.
        #expect(document.sections.flatMap(\.rows).allSatisfy { $0.leading == "☐" })

        await vault.bridge.shutdown()
    }

    /// An empty list is a document with nothing in it, and the caller is
    /// expected to beep rather than print a blank sheet.
    @Test
    func anEmptyListIsAnEmptyDocument() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.refresh()

        let document = PrintDocument.taskList(list)
        #expect(document.isEmpty)
        #expect(!PrintCommand.run(.printView, document: document), "a blank sheet costs paper")
        #expect(!PrintCommand.run(.printView, document: nil))

        await vault.bridge.shutdown()
    }

    /// A day of the calendar prints as a time-ordered list, because a grid on
    /// paper is mostly whitespace.
    @Test
    func aCalendarDayPrintsItsBlocksWithTheirTimes() async throws {
        let vault = try await TestVault()
        let calendar = CalendarModel(bridge: vault.bridge)
        await calendar.refresh()
        let start = calendar.dayStartMs(offset: 0) + 9 * 3_600_000
        await calendar.createBlock(
            fromMs: start,
            toMs: start + 3_600_000,
            title: "Deep work",
            kind: .zoned
        )

        let document = PrintDocument.calendar(calendar)
        #expect(document.title == "Calendar — Day")
        let row = try #require(document.sections.flatMap(\.rows).first)
        #expect(row.title == "Deep work")
        #expect(row.leading == "09:00", "\(row.leading)")

        await vault.bridge.shutdown()
    }

    /// The screens with no paper shape say so by producing nothing, rather
    /// than by printing a page of headings with no content under them.
    @Test
    func theScreensWithNoPaperShapeProduceNoDocument() async throws {
        let vault = try await TestVault()
        _ = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        let models = await PrintModels(bridge: vault.bridge)

        #expect(models.document(for: .focus) == nil)
        #expect(models.document(for: .routines) == nil)
        #expect(models.document(for: .morning) == nil)
        #expect(models.document(for: .evening) == nil)
        #expect(models.document(for: nil) == nil)

        #expect(models.document(for: .list(.inbox))?.title == "Inbox")
        #expect(models.document(for: .review)?.title == "Weekly Review")
        // The Calendar has a paper shape; this vault simply has no blocks in
        // it, which is the empty-document case and not the no-such-thing case.
        // The difference is exactly what `refusal` exists to state.
        #expect(PrintDocument.refusal(for: .calendar, reviewTab: .weekly) == nil)
        #expect(models.document(for: .calendar) == nil)

        await vault.bridge.shutdown()
    }

    /// Trends and History are the two the audit caught: they used to return a
    /// document carrying a title and no rows, so ⌘P produced a blank page with
    /// a heading on it instead of refusing. Both are deliberately out of
    /// scope — a chart and a list of links, each with a CSV/JSON export beside
    /// it — so the right answer is the one Focus and Routines already gave.
    @Test
    func trendsAndHistoryProduceNoDocumentRatherThanATitleOnlyPage() async throws {
        let vault = try await TestVault()
        _ = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        let models = await PrintModels(bridge: vault.bridge)

        for tab in [ReviewTab.trends, .history] {
            await models.show(tab)
            #expect(
                models.document(for: .review) == nil,
                "\(tab.title) must produce no document, not an empty one"
            )
            let reason = try #require(
                PrintDocument.refusal(for: .review, reviewTab: tab),
                "\(tab.title) must say why it cannot be printed"
            )
            #expect(reason.contains("export"), "the reason names the way out: \(reason)")
        }

        // The weekly tab still prints, so this is a refusal aimed at two tabs
        // and not a withdrawal of Review from printing altogether.
        await models.show(.weekly)
        #expect(PrintDocument.refusal(for: .review, reviewTab: .weekly) == nil)
        #expect(models.document(for: .review)?.title == "Weekly Review")

        await vault.bridge.shutdown()
    }

    /// **The invariant, over every destination there is.** A document either
    /// has rows or does not exist; the third case — a document with a title
    /// and nothing under it — is a blank page the user only discovers at the
    /// printer, and it is what Trends and History used to be. Enumerating
    /// `Destination.fixed` rather than listing screens by hand is the point:
    /// a destination added later is covered the day it is added.
    ///
    /// Every refusal also has to carry a sentence, because a greyed-out menu
    /// item with nothing to read is the accessibility failure the reason
    /// string exists to prevent.
    @Test
    func everyDestinationEitherPrintsRowsOrPrintsNothing() async throws {
        let vault = try await TestVault()
        _ = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        let models = await PrintModels(bridge: vault.bridge)

        var destinations: [Destination?] = Destination.fixed
        destinations.append(nil)

        for tab in ReviewTab.allCases {
            await models.show(tab)
            for destination in destinations {
                let label = "\(String(describing: destination)) / \(tab.title)"
                let refusal = PrintDocument.refusal(for: destination, reviewTab: tab)
                if let document = models.document(for: destination) {
                    #expect(document.rowCount > 0, "a document with no rows is a blank page: \(label)")
                    #expect(refusal == nil, "a printable screen must not also refuse: \(label)")
                } else if let refusal {
                    #expect(!refusal.isEmpty, "a refusal must say why: \(label)")
                }
                // The remaining case — no document and no refusal — is the
                // screen that can print but is empty right now. It is allowed,
                // and it is why the menu item stays enabled and beeps there.
            }
        }

        await vault.bridge.shutdown()
    }

    /// The weekly review prints its counts even before any task exists, which
    /// is what makes it a report rather than a list.
    @Test
    func theWeeklyReviewPrintsItsCounts() async throws {
        let vault = try await TestVault()
        let review = ReviewModel(bridge: vault.bridge)
        await review.refresh()

        let document = try #require(PrintDocument.review(review))
        #expect(document.title == "Weekly Review")
        let headings = document.sections.map(\.heading)
        #expect(headings.contains("This week"), "\(headings)")
        let counts = try #require(document.sections.first).rows.map(\.title)
        #expect(counts == ["Completed", "Deferred", "Dropped", "Created", "Reopened"])

        await vault.bridge.shutdown()
    }
}

/// The four models ⌘P reads, loaded, so a test can ask what any destination
/// would print without rebuilding them per case.
@MainActor
private struct PrintModels {
    let list: TaskListModel
    let search: SearchModel
    let calendar: CalendarModel
    let review: ReviewModel

    init(bridge: CoreBridge) async {
        list = TaskListModel(bridge: bridge, kind: .inbox)
        search = SearchModel(bridge: bridge)
        calendar = CalendarModel(bridge: bridge)
        review = ReviewModel(bridge: bridge)
        await list.refresh()
        await calendar.refresh()
        await review.refresh()
    }

    /// Switch tabs and let the load settle. `ReviewModel.tab`'s `didSet`
    /// starts a refresh in a detached task, so reading the report straight
    /// after assigning would be a race with it rather than a test of it.
    func show(_ tab: ReviewTab) async {
        review.tab = tab
        await review.refresh()
    }

    func document(for destination: Destination?) -> PrintDocument? {
        PrintDocument.forDestination(
            destination,
            list: list,
            search: search,
            calendar: calendar,
            review: review
        )
    }
}

/// Pagination, which is a pure function of the document so that where a page
/// breaks is something a test can read rather than something that happens
/// inside `ImageRenderer`.
struct PrintPaginationTests {
    private func document(sections: [PrintSection]) -> PrintDocument {
        PrintDocument(title: "Today", subtitle: "Monday", sections: sections)
    }

    private func rows(_ count: Int, prefix: String) -> [PrintRow] {
        (0..<count).map { PrintRow(id: "\(prefix)\($0)", title: "\(prefix) \($0)") }
    }

    @Test
    func aShortDocumentIsOnePage() {
        let pages = document(sections: [PrintSection(heading: "Now", rows: rows(3, prefix: "a"))])
            .paginated(rowsPerPage: 10)
        #expect(pages.count == 1)
        #expect(pages[0].rowCount == 3)
    }

    /// Every page carries the title and the subtitle, so page two is still
    /// identifiable when it comes off the printer on its own.
    @Test
    func everyPageRepeatsTheTitle() {
        let pages = document(sections: [PrintSection(heading: "Now", rows: rows(25, prefix: "a"))])
            .paginated(rowsPerPage: 10)
        #expect(pages.count == 3)
        #expect(pages.allSatisfy { $0.title == "Today" && $0.subtitle == "Monday" })
        #expect(pages.map(\.rowCount) == [10, 10, 5])
    }

    /// A section split across a page keeps its heading, marked as continued. A
    /// column of tasks under no heading is how a printed list stops being
    /// readable on page two.
    @Test
    func aSplitSectionRepeatsItsHeadingAsContinued() {
        let pages = document(sections: [PrintSection(heading: "Overdue", rows: rows(15, prefix: "a"))])
            .paginated(rowsPerPage: 10)
        #expect(pages.count == 2)
        #expect(pages[0].sections.map(\.heading) == ["Overdue"])
        #expect(pages[1].sections.map(\.heading) == ["Overdue (continued)"])
    }

    /// Two sections that fit together stay together, headings intact.
    @Test
    func sectionsThatFitShareAPage() {
        let pages = document(sections: [
            PrintSection(heading: "Overdue", rows: rows(3, prefix: "a")),
            PrintSection(heading: "Later", rows: rows(4, prefix: "b"))
        ]).paginated(rowsPerPage: 10)
        #expect(pages.count == 1)
        #expect(pages[0].sections.map(\.heading) == ["Overdue", "Later"])
    }

    /// Even an empty document produces one page rather than none, so a caller
    /// that ignored `isEmpty` gets a sheet saying so instead of a PDF with no
    /// pages at all — which some viewers refuse to open.
    @Test
    func anEmptyDocumentStillPaginatesToOnePage() {
        let pages = document(sections: []).paginated(rowsPerPage: 10)
        #expect(pages.count == 1)
        #expect(pages[0].isEmpty)
    }

    /// The suggested filename comes off the title, so two screens cannot
    /// propose the same file.
    @Test
    func theSuggestedFilenameIsDerivedFromTheTitle() {
        #expect(document(sections: []).suggestedFilename == "today.pdf")
        #expect(
            PrintDocument(title: "Calendar — Week", subtitle: "", sections: [])
                .suggestedFilename == "calendar-week.pdf"
        )
        #expect(
            PrintDocument(title: "@errands", subtitle: "", sections: [])
                .suggestedFilename == "errands.pdf"
        )
        #expect(PrintDocument(title: "—", subtitle: "", sections: []).suggestedFilename
            == "sunrise.pdf")
    }
}
