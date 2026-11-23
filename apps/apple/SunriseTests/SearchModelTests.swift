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

    // MARK: - Groups, highlights and saving

    /// Every kind the query found arrives in the core's order beside the
    /// task rows, each title with its matched ranges.
    @Test
    func otherKindsArriveGroupedAndHighlighted() async throws {
        let vault = try await TestVault()
        await BrowseModel(bridge: vault.bridge).createStream(name: "Passport office", color: nil)
        try await seed(vault, ["Renew passport", "Book the ferry"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))

        model.text = "passport"
        try await settled(model)

        let task = try #require(model.results.tasks.first)
        #expect(model.results.tasks.map(\.title) == ["Renew passport"])
        let taskTitle = try #require(model.taskHighlights[task.id])
        #expect(taskTitle.runs == [
            HighlightRun(text: "Renew ", matched: false),
            HighlightRun(text: "passport", matched: true)
        ])
        #expect(model.otherGroups.map(\.kind) == [.stream])
        let group = try #require(model.otherGroups.first)
        #expect(SearchModel.title(of: group).contains("1"))
        #expect(!SearchModel.isTruncated(group))
        let hit = try #require(group.hits.first)
        #expect(hit.title.runs.contains(HighlightRun(text: "Passport", matched: true)))
        #expect(SearchModel.opening(hit, in: .stream)
            == .destination(.list(.stream(id: hit.opens, name: "Passport office"))))
        await vault.bridge.shutdown()
    }

    /// ⌘S saves the canonical text, not the keystrokes, and recalling the
    /// saved search hands that query back to the field.
    @Test
    func savingASearchKeepsItsCanonicalTextAndRecallsIt() async throws {
        let vault = try await TestVault()
        try await seed(vault, ["Renew passport"])
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-views-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let views = SavedViewsModel(store: SavedViews.atPath(
            path: directory.appending(path: "views.toml").path(percentEncoded: false)
        ))

        #expect(!model.canSave, "an empty field has nothing to save")
        model.text = "  passport    STATE:todo "
        try await settled(model)
        #expect(model.canonical == "passport state:todo")
        #expect(model.canSave)

        await views.saveSearch(name: "renewals", query: model.canonical)
        let saved = try #require(views.views.first)
        #expect(saved.view == .search)
        #expect(saved.query == "passport state:todo")

        #expect(views.recall(saved, contexts: NameBook()) == .search)
        #expect(views.pendingSearch == "passport state:todo")
        views.searchTaken()
        #expect(views.pendingSearch == nil)
        await vault.bridge.shutdown()
    }

    /// "Show all" re-runs the search for one kind, and editing its operator
    /// away puts the per-group limit back.
    @Test
    func showAllNarrowsToOneKindUntilItsOperatorIsEdited() async throws {
        let vault = try await TestVault()
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))
        model.text = "office "

        model.showAll(.stream)
        #expect(model.text == "office kind:stream")
        #expect(model.expandedKind == .stream)

        model.text = "office"
        #expect(model.expandedKind == nil)
        await vault.bridge.shutdown()
    }

    // MARK: - Autocomplete and scope

    /// The suggestions are the core's, names included, and each carries a
    /// description in the reader's language.
    @Test
    func theFieldSuggestsOperatorsValuesAndNames() async throws {
        let vault = try await TestVault()
        await BrowseModel(bridge: vault.bridge).createStream(name: "Side project", color: nil)
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))
        await model.refresh()

        model.text = "deploy is:la"
        #expect(model.completions.map(\.label) == ["late", "late-hard"])
        #expect(SearchModel.describe(model.completions[0]) == L10n.Search.isLate)

        model.text = "d"
        #expect(model.completions.map(\.label) == ["due:", "done:"])
        #expect(SearchModel.describe(model.completions[0]) == L10n.Search.opDue)

        model.text = "#si"
        let name = try #require(model.completions.first)
        #expect(name.label == "#\"Side project\"")
        #expect(SearchModel.describe(name) == L10n.Search.nameStream)
        model.accept(name)
        #expect(model.text == "#\"Side project\" ")

        model.text = "passport "
        #expect(model.completions.isEmpty, "nothing is being typed after a space")
        await vault.bridge.shutdown()
    }

    /// ⌘F from a stream's list starts from its scope token, which one
    /// Backspace removes; from anywhere else the query is kept.
    @Test
    func aListsScopeIsAnEditableTokenOneBackspaceRemoves() async throws {
        let vault = try await TestVault()
        let model = SearchModel(bridge: vault.bridge, debounce: .milliseconds(1))

        model.text = "passport"
        model.open(from: .list(.todayAll))
        #expect(model.text == "passport", "Today has no scope; the query is kept")

        model.open(from: .list(.stream(id: "str_x", name: "Side project")))
        #expect(model.text == "#\"Side project\" ")
        #expect(model.scopeToken == "#\"Side project\" ")
        #expect(model.deleteBackward())
        #expect(model.text.isEmpty)
        #expect(model.scopeToken == nil)
        #expect(!model.deleteBackward(), "an empty field has no token to remove")

        model.open(from: .list(.context(id: "ctx_x", name: "errands")))
        model.text += "milk"
        #expect(model.scopeToken == "@errands ", "typing after the token keeps it")
        #expect(!model.deleteBackward(), "Backspace deletes a character, not the token")
        model.text = "milk"
        #expect(model.scopeToken == nil, "editing the token away forgets it")
        await vault.bridge.shutdown()
    }

    // MARK: - Pure helpers

    /// Ranges are Unicode-scalar offsets: a CJK title and one with an
    /// emoji are cut where the core said, and a hostile range is skipped
    /// rather than trusted.
    @Test
    func highlightRunsCutAtUnicodeScalarOffsets() {
        let cjk = HighlightedText(
            text: "Renew the passport in 東京",
            ranges: [HighlightRange(start: 10, end: 14), HighlightRange(start: 22, end: 24)]
        )
        #expect(cjk.runs == [
            HighlightRun(text: "Renew the ", matched: false),
            HighlightRun(text: "pass", matched: true),
            HighlightRun(text: "port in ", matched: false),
            HighlightRun(text: "東京", matched: true)
        ])
        let hostile = HighlightedText(
            text: "🛂 visa",
            ranges: [
                HighlightRange(start: 2, end: 6),
                HighlightRange(start: 1, end: 3),
                HighlightRange(start: 5, end: 99)
            ]
        )
        #expect(hostile.runs == [
            HighlightRun(text: "🛂 ", matched: false),
            HighlightRun(text: "visa", matched: true)
        ])
        #expect(HighlightedText(text: "", ranges: []).runs.isEmpty)
    }

    /// A stream or a context opens its list; anything else is revealed
    /// where it lives, an attachment through the entity it is on.
    @Test
    func aHitOpensItsListOrWhereItLives() {
        func hit(_ entity: EntityRef, opens: EntityRef, _ title: String) -> SearchHitItem {
            SearchHitItem(
                entity: entity,
                opens: opens,
                title: HighlightedText(text: title, ranges: []),
                snippet: nil
            )
        }
        #expect(SearchModel.opening(hit("ctx_1", opens: "ctx_1", "errands"), in: .context)
            == .destination(.list(.context(id: "ctx_1", name: "errands"))))
        #expect(SearchModel.opening(hit("att_1", opens: "tsk_1", "scan.pdf"), in: .attachment)
            == .reveal("tsk_1"))
        #expect(SearchModel.opening(hit("blk_1", opens: "blk_1", "Deep work"), in: .block)
            == .reveal("blk_1"))
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
