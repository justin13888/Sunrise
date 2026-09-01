import Foundation

/// One Block an import wrote.
struct IcalImportedRow: Identifiable, Hashable, Sendable {
    let id: EntityRef
    let uid: String
    let title: String
    /// `true` when this event minted the Block, `false` when it landed on one
    /// an earlier import already made. The distinction is the visible half of
    /// idempotence: a second import of the same file shows fifteen "already
    /// here" rows and no duplicates.
    let isNew: Bool
}

/// The notices of one kind, under the heading that explains them.
struct IcalNoticeGroup: Identifiable, Hashable, Sendable {
    let code: NoticeCode
    /// What the sheet puts above the lines.
    let title: String
    /// One line per notice: the `UID` it happened in, then the detail the core
    /// already phrased.
    let lines: [String]
    /// Whether these describe a whole item being dropped rather than a
    /// property that did not come across.
    ///
    /// The same split `ical_vault::ImportReport::dropped()` makes, and it
    /// decides ordering and emphasis: a `VTODO` that never became anything is
    /// a different size of loss from a `DESCRIPTION` that was not carried.
    let isLoss: Bool

    var id: String { title }
}

/// What an import did, phrased for a human.
///
/// A value, built from `IcalImportReport` and holding no vault: the whole point
/// of the sheet is the notices, and a summary that could only be checked by
/// driving a file picker is one nobody checks.
struct IcalImportSummary: Equatable, Sendable {
    let created: Int
    let updated: Int
    let failed: Int
    let blocks: [IcalImportedRow]
    let notices: [IcalNoticeGroup]

    /// The order the sheet prints the groups in, losses first.
    ///
    /// Fixed rather than derived from the file, so two imports of similar
    /// documents do not reshuffle the sheet under the reader.
    private static let codeOrder: [NoticeCode] = [
        .skipped, .unsupportedComponent, .badValue, .unknownTimezone, .unmappedProperty
    ]

    init(report: IcalImportReport) {
        created = Int(report.created)
        updated = Int(report.updated)
        failed = Int(report.failed)
        blocks = report.blocks.map {
            IcalImportedRow(id: $0.block, uid: $0.uid, title: $0.title, isNew: $0.created)
        }
        notices = Self.codeOrder.compactMap { code in
            let matching = report.notices.filter { $0.code == code }
            guard !matching.isEmpty else { return nil }
            return IcalNoticeGroup(
                code: code,
                title: Self.heading(for: code),
                lines: matching.map { notice in
                    guard let uid = notice.uid, !uid.isEmpty else { return notice.detail }
                    return "\(uid): \(notice.detail)"
                },
                isLoss: Self.isLoss(code)
            )
        }
    }

    /// The one line at the top. The same four numbers `ImportReport::
    /// summary_line` gives the CLI, said the way a sheet says them.
    var headline: String {
        var parts = [created == 1 ? "1 event imported" : "\(created) events imported"]
        if updated > 0 { parts.append("\(updated) already up to date") }
        if failed > 0 {
            parts.append(failed == 1 ? "1 could not be read" : "\(failed) could not be read")
        }
        return parts.joined(separator: ", ")
    }

    /// How many individual notices there are, across every group.
    var noticeCount: Int { notices.reduce(0) { $0 + $1.lines.count } }

    /// Whether anything was lost on the way in.
    var hasNotices: Bool { !notices.isEmpty }

    /// The sentence under the headline. Present even when nothing was lost:
    /// "everything came across" is the answer to the question the sheet is
    /// there to ask, and leaving it blank makes silence ambiguous.
    var noticeSummary: String {
        guard hasNotices else { return "Everything in the file came across." }
        let count = noticeCount
        return count == 1
            ? "1 thing could not be carried into a time block."
            : "\(count) things could not be carried into time blocks."
    }

    /// `docs/09-integrations/icalendar.md` §Edge cases: the whole-item losses
    /// are the subset a client must show even if it hides the rest.
    private static func isLoss(_ code: NoticeCode) -> Bool {
        switch code {
        case .skipped, .unsupportedComponent: true
        case .badValue, .unknownTimezone, .unmappedProperty: false
        }
    }

    private static func heading(for code: NoticeCode) -> String {
        switch code {
        case .skipped: "Events not imported"
        case .unsupportedComponent: "Parts of the file a time block cannot hold"
        case .badValue: "Values that could not be read"
        case .unknownTimezone: "Unknown time zones, read as UTC"
        case .unmappedProperty: "Details that did not come across"
        }
    }
}

/// Reading and writing `.ics`.
///
/// Both halves are the core's — `SunriseCore::import_ical` parses and
/// `::export_ical` renders — and both cross as **text**, because on macOS the
/// file the user picked carries a security scope only this process holds and
/// the save panel's destination is likewise this process's to write. Reading
/// and writing bytes is the app's job; understanding them is not.
@MainActor
@Observable
final class IcalModel {
    /// What the last import did, or `nil` when none has run since the sheet
    /// was dismissed. Non-`nil` is what puts the report on screen.
    var summary: IcalImportSummary?

    /// Why the last import or export could not run at all. Distinct from a
    /// notice: a notice is part of a successful import.
    var errorMessage: String?

    /// Whether a call is in flight, so a menu item can be disabled rather than
    /// queueing a second read of the same file.
    private(set) var isBusy = false

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// Read one `.ics` document into the vault.
    ///
    /// **`source` is deliberately not passed.** The Block an event lands on is
    /// `imported_block_id(source, UID)` — BLAKE3 over the pair — so the source
    /// is half of the identity that makes a re-import update rather than
    /// duplicate. Leaving it `nil` takes `ical_vault::ICS_SOURCE`, the one
    /// shared name every one-shot `.ics` uses and the same one `sunrise ical
    /// import` defaults to. Passing anything derived from *this run* — the
    /// file's path, its name, a timestamp — would make every import a new
    /// identity space, and re-importing `~/Downloads/basic (1).ics` would give
    /// the user a second copy of their whole calendar.
    ///
    /// `stream` is where the Blocks are filed; `nil` is the Inbox, which is
    /// what a file picker with no other information should choose.
    func importDocument(text: String, into stream: EntityRef? = nil) async {
        isBusy = true
        defer { isBusy = false }
        do {
            let report = try await bridge.importIcal(text: text, into: stream)
            summary = IcalImportSummary(report: report)
            errorMessage = nil
        } catch {
            summary = nil
            errorMessage = error.localizedDescription
        }
    }

    /// Render one window of the calendar as an `.ics` document.
    ///
    /// Returns the text rather than writing it, so the caller can hand it to a
    /// save panel it already holds the destination for. `nil` means the export
    /// failed and ``errorMessage`` says why.
    func exportDocument(window: ExportWindow) async -> String? {
        isBusy = true
        defer { isBusy = false }
        do {
            let text = try await bridge.exportIcal(window: window)
            errorMessage = nil
            return text
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    func dismissSummary() { summary = nil }
    func dismissError() { errorMessage = nil }
}

/// The two windows the export offers, named for the menu.
///
/// `ExportWindow` is the seam's enum and gets no extension of its own here
/// beyond this: it has to stay the core's vocabulary, and a client that added
/// a third case would be inventing a window the core cannot render.
extension ExportWindow {
    /// The order the Export menu lists them in — narrowest first, which is the
    /// one somebody exporting an agenda reaches for.
    static let menuOrder: [ExportWindow] = [.day, .week]

    var menuTitle: String {
        switch self {
        case .day: "Today"
        case .week: "This Week"
        }
    }

    /// What the save panel proposes as a filename.
    var suggestedFilename: String {
        switch self {
        case .day: "sunrise-today.ics"
        case .week: "sunrise-week.ics"
        }
    }
}
