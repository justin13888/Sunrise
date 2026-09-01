import Foundation
import Testing

@testable import Sunrise

/// Review against a real vault: the four reports, the snapshot, the export.
@MainActor
struct ReviewModelTests {
    @Test
    func theWeeklyReviewCountsWhatHappened() async throws {
        let vault = try await TestVault()
        let done = try await create(vault, "Renew passport")
        _ = try await create(vault, "Book the ferry")
        _ = try await vault.bridge.submit(.completeTask(id: done))

        let model = ReviewModel(bridge: vault.bridge)
        await model.refresh()

        let weekly = try #require(model.weekly)
        #expect(weekly.totals.created == 2)
        #expect(weekly.totals.completed == 1)
        #expect(weekly.window.endMs > weekly.window.startMs)
        await vault.bridge.shutdown()
    }

    /// `blocked` is the part of *today's* plan that is waiting on something
    /// else — not every blocked task in the vault. The core scopes it that way
    /// because the glance is about today, and this pins that the app shows it
    /// scoped rather than reinterpreting it.
    @Test
    func theDailyGlanceShowsWhichOfTodaysPlanIsBlocked() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        let blocker = try await create(vault, "Draft the letter")
        let waiting = try await create(vault, "Post the letter", blockedBy: [blocker])
        var scheduled = TaskEdit()
        scheduled.setScheduledAt = .instant(at: Timestamp(now))
        _ = try await vault.bridge.submit(.updateTask(id: waiting, edit: scheduled))

        let model = ReviewModel(bridge: vault.bridge)
        model.tab = .daily
        await model.refresh()

        let daily = try #require(model.daily)
        #expect(daily.today.map(\.title) == ["Post the letter"])
        #expect(daily.blocked.map(\.title) == ["Post the letter"])
        #expect(daily.inbox.contains { $0.title == "Draft the letter" })
        await vault.bridge.shutdown()
    }

    @Test
    func trendsCoverTheWeeksAskedFor() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Renew passport")
        let model = ReviewModel(bridge: vault.bridge)
        model.tab = .trends
        model.weeks = 4
        await model.refresh()

        let trends = try #require(model.trends)
        #expect(trends.overall.count == 4)
        #expect(trends.weekStarts.count == 4)
        await vault.bridge.shutdown()
    }

    /// A snapshot is the one fact a review leaves behind that the op log
    /// cannot re-derive: that someone did it, and what they saw.
    @Test
    func savingASnapshotRecordsTheCountsTheScreenShowed() async throws {
        let vault = try await TestVault()
        let done = try await create(vault, "Renew passport")
        _ = try await vault.bridge.submit(.completeTask(id: done))
        let model = ReviewModel(bridge: vault.bridge)
        await model.refresh()
        let shown = try #require(model.weekly).totals

        await model.saveSnapshot(note: "quiet week")

        model.tab = .history
        await model.refresh()
        let snapshot = try #require(model.snapshots.first)
        #expect(snapshot.totals.completed == shown.completed)
        #expect(snapshot.totals.created == shown.created)
        #expect(snapshot.note == "quiet week")
        await vault.bridge.shutdown()
    }

    @Test
    func anEmptyNoteIsNoNoteAtAll() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Renew passport")
        let model = ReviewModel(bridge: vault.bridge)
        await model.refresh()

        await model.saveSnapshot(note: "   ")

        model.tab = .history
        await model.refresh()
        #expect(try #require(model.snapshots.first).note == nil)
        await vault.bridge.shutdown()
    }

    /// The export document is the core's, byte for byte. This only checks the
    /// app can reach it and names the file the way the CLI names it.
    @Test
    func exportProducesTheCoresDocument() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Renew passport")
        let model = ReviewModel(bridge: vault.bridge)
        await model.refresh()

        let csv = try #require(await model.export(.trends, as: .csv))
        #expect(csv.contains(","))
        let json = try #require(await model.export(.trends, as: .json))
        #expect(json.hasPrefix("{") || json.hasPrefix("["), "\(json.prefix(20))")

        let name = model.exportFilename(.trends, as: .csv)
        #expect(name.hasPrefix("sunrise-trends-"))
        #expect(name.hasSuffix(".csv"))
        #expect(ExportDataset.trends.title == "Trends")
        await vault.bridge.shutdown()
    }

    /// Every dataset the seam offers is reachable from the export menu, in
    /// both formats. A dataset added upstream and not offered here would be
    /// exactly the "correct and unreachable" failure this epic keeps finding.
    @Test
    func everyDatasetAndFormatIsReachable() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Renew passport")
        let model = ReviewModel(bridge: vault.bridge)
        await model.refresh()

        for dataset in ExportDataset.all {
            for format in ExportFormat.all {
                let body = await model.export(dataset, as: format)
                #expect(body != nil, "\(exportDatasetName(dataset: dataset)) as \(format)")
            }
        }
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    private func create(
        _ vault: borrowing TestVault,
        _ title: String,
        blockedBy: [EntityRef] = []
    ) async throws -> EntityRef {
        let outcome = try await vault.bridge.submit(.createTask(draft: TaskDraftIn(
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
        )))
        if !blockedBy.isEmpty {
            var edit = TaskEdit()
            edit.blockedBy = blockedBy
            _ = try await vault.bridge.submit(.updateTask(id: outcome.entity, edit: edit))
        }
        return outcome.entity
    }
}
