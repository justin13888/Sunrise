#if os(macOS)
import Foundation
import Testing

@testable import Sunrise

/// File → Export Everything…'s Swift side: the folder name it suggests, what
/// the finished sheet says, and the two refusals that keep a running export on
/// screen.
///
/// The whole suite is macOS-only, as `HotkeyStatusTests` is: the menu item,
/// its save panel and its sheet live in `apps/apple/macOS/`, and iOS has no
/// Export Everything surface (`docs/04-storage/export-format.md` §Not yet).
/// The export itself is the core's and is tested there
/// (`crates/sunrise-client-core/tests/export.rs`).
@MainActor
struct ExportEverythingTests {
    private let folder = URL(filePath: "/tmp/Sunrise Export 2026-11-07", directoryHint: .isDirectory)

    private func wrote(files: UInt32, missing: UInt32 = 0, fetchFailed: UInt32 = 0) -> FullExportSummary {
        FullExportSummary(
            files: files,
            attachmentsPresent: 1,
            attachmentsMissing: missing,
            attachmentsFetchFailed: fetchFailed
        )
    }

    /// The date in the name is the one on the user's clock, not UTC's: at
    /// 23:30 in Los Angeles it is still the 6th there and already the 7th in
    /// Greenwich.
    @Test
    func theSuggestedFolderIsDatedInTheUsersZone() throws {
        let lateEvening = Date(timeIntervalSince1970: 1_794_036_600) // 2026-11-07T07:30:00Z
        let losAngeles = try #require(TimeZone(identifier: "America/Los_Angeles"))
        let utc = try #require(TimeZone(identifier: "UTC"))

        let inLosAngeles = ExportEverythingPanel.suggestedName(now: lateEvening, zone: losAngeles)
        let inUTC = ExportEverythingPanel.suggestedName(now: lateEvening, zone: utc)

        #expect(inLosAngeles == "Sunrise Export 2026-11-06")
        #expect(inUTC == "Sunrise Export 2026-11-07")
    }

    /// Nothing left out: the files, and a line saying nothing was, with no
    /// "export again" line, because exporting again would not help.
    @Test
    func aCompleteExportSaysNothingWasLeftOut() {
        let summary = wrote(files: 12)

        #expect(ExportEverythingSheet.lines(for: summary, folder: folder) == [
            "Wrote 12 files to “Sunrise Export 2026-11-07”.",
            "No attachments were left out."
        ])
    }

    /// Bytes left out for a reason a fetch cannot fix (deleted, say) are
    /// counted, and still no "export again" line.
    @Test
    func attachmentsLeftOutWithoutAFailedFetchAreCountedButNotRetried() {
        let summary = wrote(files: 1, missing: 1)

        #expect(ExportEverythingSheet.lines(for: summary, folder: folder) == [
            "Wrote 1 file to “Sunrise Export 2026-11-07”.",
            "1 attachment was left out; the folder’s manifest.json lists it."
        ])
    }

    /// A fetch that failed is the one case where exporting again online would
    /// bring the bytes, so it is the one case the sheet says so.
    @Test
    func aFailedFetchAddsTheExportAgainLine() {
        let summary = wrote(files: 5, missing: 3, fetchFailed: 2)

        #expect(ExportEverythingSheet.lines(for: summary, folder: folder) == [
            "Wrote 5 files to “Sunrise Export 2026-11-07”.",
            "3 attachments were left out; the folder’s manifest.json lists them.",
            "2 could not be downloaded. Export again while online to include them."
        ])
    }

    /// The call cannot be cancelled across the seam, so the sheet that says it
    /// is going on cannot be put away while it is.
    @Test
    func aRunningExportCannotBeDismissed() {
        let model = ExportEverythingModel(phase: .running(folder: folder))

        model.dismiss()

        #expect(model.isRunning)
    }

    /// Once it has finished, or failed, Done puts the sheet away.
    @Test
    func aFinishedOrFailedExportIsDismissed() {
        let finished = ExportEverythingModel(phase: .finished(folder: folder, summary: wrote(files: 1)))
        let failed = ExportEverythingModel(phase: .failed(message: "The destination is not empty."))

        finished.dismiss()
        failed.dismiss()

        #expect(finished.phase == nil)
        #expect(failed.phase == nil)
    }

    /// A second start while one runs is refused before the save panel opens,
    /// even with a vault to export: the running export's folder is still the
    /// one on screen. Were the refusal not there, this test would block on a
    /// modal panel nobody answers.
    @Test
    func aSecondStartWhileOneRunsIsRefused() async throws {
        let vault = try await TestVault()
        let model = ExportEverythingModel(phase: .running(folder: folder))

        await model.start(from: vault.bridge)

        guard case let .running(stillRunning) = model.phase else {
            Issue.record("the first export should still be running: \(String(describing: model.phase))")
            await vault.bridge.shutdown()
            return
        }
        #expect(stillRunning == folder)
        await vault.bridge.shutdown()
    }

    /// With no vault open there is nothing to export, so start is refused
    /// before the panel too.
    @Test
    func startWithNoVaultIsRefused() async {
        let model = ExportEverythingModel()

        await model.start(from: nil)

        #expect(model.phase == nil)
    }
}
#endif
