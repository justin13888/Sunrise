import Foundation
import Testing

@testable import Sunrise

/// A document with one clean event and one of everything a Block cannot hold:
/// a recurrence, an alarm, an attendee, a description, a `VTODO`, and a
/// timezone reference no Mac has ever heard of.
///
/// Written out rather than read from `crates/sunrise-integrations/testdata`:
/// this asserts what *this client* does with a report, and a fixture the Rust
/// tests are free to change is a fixture that would break this file for
/// reasons that have nothing to do with it.
private let mixedDocument = """
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Sunrise//macOS tests//EN
BEGIN:VEVENT
UID:clean@example.com
DTSTAMP:20260301T120000Z
DTSTART:20260302T090000Z
DTEND:20260302T100000Z
SUMMARY:Quarterly planning
END:VEVENT
BEGIN:VEVENT
UID:busy@example.com
DTSTAMP:20260301T120000Z
DTSTART;TZID=Mars/Olympus:20260303T140000
DTEND;TZID=Mars/Olympus:20260303T150000
SUMMARY:Weekly sync
DESCRIPTION:Bring the numbers.
RRULE:FREQ=WEEKLY;BYDAY=TU
ATTENDEE;CN=Sam:mailto:sam@example.com
BEGIN:VALARM
ACTION:DISPLAY
TRIGGER:-PT15M
DESCRIPTION:Reminder
END:VALARM
END:VEVENT
BEGIN:VTODO
UID:todo@example.com
DTSTAMP:20260301T120000Z
SUMMARY:Renew the domain
END:VTODO
END:VCALENDAR
"""

/// The iCalendar surface: the seam wrapper, the model, and the summary the
/// sheet prints.
@MainActor
struct IcalModelTests {
    /// The MUST, end to end. Blocks land, and every part of the file a Block
    /// cannot hold is named.
    @Test
    func importingADocumentWritesBlocksAndNamesWhatItCouldNotCarry() async throws {
        let vault = try await TestVault()
        let model = IcalModel(bridge: vault.bridge)

        await model.importDocument(text: mixedDocument)

        #expect(model.errorMessage == nil)
        let summary = try #require(model.summary)
        #expect(summary.created == 2, "two VEVENTs: \(summary)")
        #expect(summary.updated == 0)
        #expect(summary.blocks.map(\.title).sorted() == ["Quarterly planning", "Weekly sync"])
        #expect(summary.blocks.allSatisfy { $0.isNew })

        // The whole reason the sheet exists.
        #expect(summary.hasNotices, "an RRULE, a VALARM, an ATTENDEE and a VTODO were all dropped")
        let everything = summary.notices.flatMap(\.lines).joined(separator: "\n")
        #expect(everything.contains("VTODO"), "\(everything)")
        #expect(everything.contains("VALARM"), "\(everything)")
        #expect(everything.lowercased().contains("rrule"), "\(everything)")
        #expect(everything.contains("Mars/Olympus"), "the unknown TZID must be named: \(everything)")

        await vault.bridge.shutdown()
    }

    /// **Idempotence, which the source argument owns.**
    ///
    /// The Block an event lands on is BLAKE3 over `(source, UID)`. The wrapper
    /// passes no source at all so the core's shared `ics` name is used on
    /// every run; anything derived from *this* run — the file's path, its
    /// name, a timestamp — would make each import its own identity space and
    /// hand the user a second copy of their calendar.
    @Test
    func reimportingTheSameDocumentUpdatesRatherThanDuplicating() async throws {
        let vault = try await TestVault()
        let model = IcalModel(bridge: vault.bridge)

        await model.importDocument(text: mixedDocument)
        let first = try #require(model.summary)
        let firstIDs = first.blocks.map(\.id).sorted()

        await model.importDocument(text: mixedDocument)
        let second = try #require(model.summary)

        #expect(second.created == 0, "nothing new the second time: \(second)")
        #expect(second.updated == 2)
        #expect(second.blocks.map(\.id).sorted() == firstIDs, "the same Blocks, not a second set")
        #expect(second.blocks.allSatisfy { !$0.isNew })
        #expect(second.headline == "0 events imported, 2 already up to date")

        await vault.bridge.shutdown()
    }

    /// A document this app wrote, read back by this app, lands on the Blocks
    /// it came from. Every exported `UID` is the Block's own id, so the round
    /// trip is the sharpest test of both halves at once.
    @Test
    func exportingAndReimportingLandsOnTheSameBlocks() async throws {
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

        let model = IcalModel(bridge: vault.bridge)
        let document = try #require(await model.exportDocument(window: .day))
        #expect(document.contains("BEGIN:VCALENDAR"))
        #expect(document.contains("Deep work"), "\(document)")

        await model.importDocument(text: document)
        let summary = try #require(model.summary)
        #expect(summary.created == 0, "the round trip must not double the day: \(summary)")
        #expect(summary.updated == 1)

        await vault.bridge.shutdown()
    }

    /// A week export covers a week. The two windows are the seam's, and this
    /// is what makes the menu's two items mean different things.
    @Test
    func theTwoExportWindowsAreDifferentDocuments() async throws {
        let vault = try await TestVault()
        let calendar = CalendarModel(bridge: vault.bridge)
        await calendar.refresh()
        let today = calendar.dayStartMs(offset: 0)
        // The model's own week window is Monday-first and anchored on the same
        // instant the export is, so a day taken from it is a day the week
        // export covers — whichever weekday the test happens to run on.
        calendar.span = .week
        await calendar.refresh()
        let otherDay = try #require(
            (0..<7).map { calendar.dayStartMs(offset: $0) }.first { $0 != today }
        )
        let later = otherDay + 10 * 3_600_000
        await calendar.createBlock(
            fromMs: later,
            toMs: later + 3_600_000,
            title: "Later this week",
            kind: .zoned
        )

        let model = IcalModel(bridge: vault.bridge)
        let week = try #require(await model.exportDocument(window: .week))
        let day = try #require(await model.exportDocument(window: .day))

        #expect(week.contains("Later this week"), "\(week)")
        #expect(!day.contains("Later this week"), "a day export must not carry the week: \(day)")

        await vault.bridge.shutdown()
    }

    /// Text that is not a calendar is a failure to report, not a crash and not
    /// an empty success.
    @Test
    func textThatIsNotACalendarIsReported() async throws {
        let vault = try await TestVault()
        let model = IcalModel(bridge: vault.bridge)

        await model.importDocument(text: "Dear diary,\n")

        #expect(model.summary == nil)
        #expect(model.errorMessage != nil)
        model.dismissError()
        #expect(model.errorMessage == nil)

        await vault.bridge.shutdown()
    }
}

/// The report, phrased. No vault: this is the part that decides what the user
/// reads, and it must be checkable without driving a file picker.
struct IcalImportSummaryTests {
    private func notice(_ code: NoticeCode, _ uid: String?, _ detail: String) -> IcalNotice {
        IcalNotice(code: code, uid: uid, detail: detail)
    }

    private func block(_ id: String, _ title: String, isNew: Bool) -> IcalImportedBlock {
        IcalImportedBlock(block: id, uid: "\(title)@example.com", title: title, created: isNew)
    }

    @Test
    func theHeadlineCountsWhatHappenedAndOmitsWhatDidNot() {
        let clean = IcalImportSummary(
            report: IcalImportReport(
                blocks: [block("blk_1", "One", isNew: true)],
                created: 1,
                updated: 0,
                failed: 0,
                notices: []
            )
        )
        #expect(clean.headline == "1 event imported")
        #expect(!clean.hasNotices)
        #expect(clean.noticeSummary == "Everything in the file came across.")

        let messy = IcalImportSummary(
            report: IcalImportReport(
                blocks: [],
                created: 3,
                updated: 12,
                failed: 1,
                notices: [notice(.skipped, "a@example.com", "VTODO is not a time block")]
            )
        )
        #expect(messy.headline == "3 events imported, 12 already up to date, 1 could not be read")
        #expect(messy.noticeSummary == "1 thing could not be carried into a time block.")
    }

    /// Grouped by kind and ordered losses-first, so the reader meets "this
    /// event never arrived" before "this description did not come across".
    @Test
    func noticesAreGroupedAndTheWholeItemLossesComeFirst() {
        let summary = IcalImportSummary(
            report: IcalImportReport(
                blocks: [],
                created: 0,
                updated: 0,
                failed: 1,
                notices: [
                    notice(.unmappedProperty, "b@example.com", "DESCRIPTION not held"),
                    notice(.unknownTimezone, "b@example.com", "Mars/Olympus read as UTC"),
                    notice(.skipped, "a@example.com", "VTODO is not a time block"),
                    notice(.unmappedProperty, "c@example.com", "ATTENDEE not held"),
                    notice(.unsupportedComponent, nil, "VALARM ignored")
                ]
            )
        )

        #expect(summary.notices.map(\.code) == [
            .skipped, .unsupportedComponent, .unknownTimezone, .unmappedProperty
        ])
        #expect(summary.notices.prefix(2).allSatisfy { $0.isLoss })
        #expect(summary.notices.suffix(2).allSatisfy { !$0.isLoss })
        #expect(summary.noticeCount == 5)
        #expect(summary.notices.first?.lines == ["a@example.com: VTODO is not a time block"])
        // No `UID` means no prefix, rather than a dangling colon.
        #expect(summary.notices[1].lines == ["VALARM ignored"])
        #expect(summary.notices.allSatisfy { !$0.title.isEmpty })
    }

    /// The new/updated split is idempotence made visible. A sheet that only
    /// printed a total would make a second import look like a duplication.
    @Test
    func everyWrittenBlockSaysWhetherItWasNew() {
        let summary = IcalImportSummary(
            report: IcalImportReport(
                blocks: [block("blk_1", "One", isNew: true), block("blk_2", "Two", isNew: false)],
                created: 1,
                updated: 1,
                failed: 0,
                notices: []
            )
        )
        #expect(summary.blocks.map(\.isNew) == [true, false])
        #expect(summary.blocks.map(\.id) == ["blk_1", "blk_2"])
    }

    /// Both windows are offered, and both are named.
    @Test
    func theExportMenuOffersBothWindows() {
        #expect(ExportWindow.menuOrder == [.day, .week])
        #expect(ExportWindow.day.menuTitle == "Today")
        #expect(ExportWindow.week.menuTitle == "This Week")
        #expect(ExportWindow.menuOrder.allSatisfy { $0.suggestedFilename.hasSuffix(".ics") })
    }
}

/// Reading and writing the file itself, which is the app's half of the seam.
struct IcalFilesTests {
    private func temporaryFile(named name: String, bytes: Data) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
            .appending(path: name)
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try bytes.write(to: url)
        return url
    }

    @Test
    func aUtf8DocumentReadsBack() throws {
        let url = try temporaryFile(named: "basic.ics", bytes: Data(mixedDocument.utf8))
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        #expect(try IcalFiles.read(url) == mixedDocument)
    }

    /// An `.ics` written by something old is still an `.ics`. Refusing it
    /// outright would be this client deciding a file is unreadable when the
    /// parser has not been asked.
    @Test
    func aLatin1DocumentIsReadRatherThanRefused() throws {
        let body = "BEGIN:VCALENDAR\nSUMMARY:Caf\u{00E9}\nEND:VCALENDAR"
        let bytes = try #require(body.data(using: .isoLatin1))
        let url = try temporaryFile(named: "legacy.ics", bytes: bytes)
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }

        #expect(try IcalFiles.read(url).contains("Café"))
    }

    @Test
    func writingAnExportRoundTrips() throws {
        let url = try temporaryFile(named: "out.ics", bytes: Data())
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }

        try IcalFiles.write(mixedDocument, to: url)
        #expect(try IcalFiles.read(url) == mixedDocument)
    }

    /// The open and save panels must offer `.ics` at all — a picker filtered to
    /// nothing shows the user an empty folder.
    @Test
    func thePanelsOfferIcsDocuments() {
        let extensions = IcalFiles.documentTypes.flatMap(\.tags.values).flatMap { $0 }
        #expect(extensions.contains("ics"), "\(extensions)")
    }
}
