import Foundation
import Testing

@testable import Sunrise

/// Search against a real vault, through the same FTS query `sunrise-cli`
/// uses.
@MainActor
struct SearchModelTests {
    @Test
    func typingNarrowsTheResults() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport", "Renew library card", "Book the ferry"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))

        model.text = "renew"
        try await settled(model)
        #expect(model.results.tasks.count == 2)

        model.text = "renew passport"
        try await settled(model)
        #expect(model.results.tasks.map(\.title) == ["Renew passport"])
        await vault.bridge.shutdown()
    }

    /// An empty field is not a query. `Query::Search` over an empty string is
    /// a scan whose answer nobody asked for.
    @Test
    func anEmptyFieldRunsNoQuery() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))

        model.text = "renew"
        try await settled(model)
        #expect(!model.results.tasks.isEmpty)

        model.text = "   "
        try await settled(model)
        #expect(model.results.tasks.isEmpty)
        #expect(model.results.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func clearingEmptiesTheFieldAndTheResults() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))
        model.text = "passport"
        try await settled(model)

        model.clear()
        try await until { model.results.tasks.isEmpty }

        #expect(model.text.isEmpty)
        #expect(model.searchedText.isEmpty)
        await vault.bridge.shutdown()
    }

    /// The empty state must name the word the results answer, not the word
    /// half-typed into the field a moment ago.
    @Test
    func theEmptyStateNamesWhatWasActuallySearchedFor() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))

        model.text = "nonesuch"
        try await settled(model)

        #expect(model.results.tasks.isEmpty)
        #expect(model.searchedText == "nonesuch")
        #expect(model.results.kind.emptyMessage.contains("nonesuch"))
        await vault.bridge.shutdown()
    }

    /// A result is a real row: completing one from search goes through the
    /// same path as completing one from Today.
    @Test
    func aTaskCanBeCompletedFromASearchResult() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))
        model.text = "passport"
        try await settled(model)

        await model.results.complete(try #require(model.results.tasks.first))

        #expect(model.results.tasks.first?.state == .done)
        await vault.bridge.shutdown()
    }

    /// A word typed and then replaced must not have its late answer overwrite
    /// the newer one.
    @Test
    func aSupersededQueryDoesNotLandLate() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport", "Book the ferry"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(60))

        model.text = "passport"
        model.text = "ferry"
        try await Task.sleep(for: .milliseconds(400))

        #expect(model.searchedText == "ferry")
        #expect(model.results.tasks.map(\.title) == ["Book the ferry"])
        await vault.bridge.shutdown()
    }

    // MARK: - Helpers

    private func seed(_ vault: borrowing TestVault, _ titles: [String]) async throws {
        for title in titles {
            _ = try await vault.bridge.submit(.createTask(draft: TaskDraftIn(
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
        }
    }

    private func settled(_ model: SearchModel) async throws {
        try await until { model.searchedText == model.text && !model.isSearching }
    }

    private func until(
        _ condition: @MainActor () -> Bool,
        timeout: Duration = .seconds(3)
    ) async throws {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while ContinuousClock.now < deadline {
            if condition() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("condition never became true within \(timeout)")
    }
}
