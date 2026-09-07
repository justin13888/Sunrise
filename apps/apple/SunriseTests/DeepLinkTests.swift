import Foundation
import Testing

@testable import Sunrise

/// `sunrise://` parsing.
///
/// `docs/07-clients/interaction-patterns.md` ends its URL-scheme section with
/// "All deep links are validated; unknown shapes are ignored", so most of what
/// follows is about the *refusals*: a link the app half-understands is worse
/// than one it drops.
struct DeepLinkTests {
    private let task = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV"
    private let block = "blk_01ARZ3NDEKTSV4RRFFQ69G5FAV"

    private func link(_ string: String) -> DeepLink? {
        guard let url = URL(string: string) else { return nil }
        return DeepLink(url: url)
    }

    @Test
    func theTwoBriefsHaveALinkEach() {
        #expect(link("sunrise://morning") == .morningSummary)
        #expect(link("sunrise://evening") == .endOfDayPlan)
    }

    /// The whole point of a deep-linked view: the link names a screen.
    @Test
    func eachBriefLinkNamesItsScreen() {
        #expect(DeepLink.morningSummary.destination == .morning)
        #expect(DeepLink.endOfDayPlan.destination == .evening)
    }

    /// The three shapes `interaction-patterns.md` §Action wiring names, so a
    /// link written by any other client is understood here.
    @Test
    func theDocumentedTaskActionsParse() {
        #expect(link("sunrise://task/\(task)?action=complete") == .task(task, .complete))
        #expect(link("sunrise://task/\(task)?action=snooze_1h") == .task(task, .snooze(.oneHour)))
        #expect(
            link("sunrise://task/\(task)?action=snooze_tomorrow")
                == .task(task, .snooze(.tomorrow))
        )
    }

    /// No `action` is the body tap, which is the common case.
    @Test
    func aTaskLinkWithNoActionOpensIt() {
        #expect(link("sunrise://task/\(task)") == .task(task, .open))
    }

    /// Acting on a task must not drag a window forward:
    /// "translates to a CRDT op without opening UI when possible".
    @Test
    func onlyTheOpenActionGoesToAScreen() {
        #expect(DeepLink.task(task, .open).destination == .list(.todayAll))
        #expect(DeepLink.task(task, .complete).destination == nil)
        #expect(DeepLink.task(task, .snooze(.tomorrow)).destination == nil)
    }

    /// A block belongs on the grid, a task on Today — which screen the link
    /// opens. Which *thing* it then shows is ``DeepLink/reveal``, below.
    @Test
    func anEntityLinkLandsWhereThatKindLives() {
        #expect(link("sunrise://entity/\(block)")?.destination == .calendar)
        #expect(link("sunrise://entity/\(task)")?.destination == .list(.todayAll))
    }

    /// **The half of issue #40 a user could see.** A link that names an
    /// entity has to carry it past the screen: the calendar opens on today
    /// and Today holds only today, so a block reminder for Thursday and a
    /// task not scheduled for now both landed on a screen with no sign of
    /// what the alert was about.
    @Test
    func aLinkThatNamesAnEntityRevealsIt() {
        #expect(link("sunrise://entity/\(block)")?.reveal == block)
        #expect(link("sunrise://entity/\(task)")?.reveal == task)
        #expect(link("sunrise://task/\(task)")?.reveal == task)
    }

    /// Nothing else names one. A completion is not a screen, and the Focus
    /// screen shows the session it just started without being told twice.
    @Test
    func aLinkThatNamesNoEntityRevealsNothing() {
        #expect(DeepLink.morningSummary.reveal == nil)
        #expect(DeepLink.endOfDayPlan.reveal == nil)
        #expect(DeepLink.capture(text: "Buy milk").reveal == nil)
        #expect(DeepLink.task(task, .complete).reveal == nil)
        #expect(DeepLink.task(task, .snooze(.oneHour)).reveal == nil)
        #expect(DeepLink.focus(task).reveal == nil)
    }

    /// `sunrise://focus/<TaskId>`, which the document has named since before
    /// there was a parser. One task, because a session is time spent on one
    /// piece of work.
    @Test
    func aFocusLinkNamesATaskAndTheFocusScreen() {
        #expect(link("sunrise://focus/\(task)") == .focus(task))
        #expect(DeepLink.focus(task).destination == .focus)
    }

    /// The `stream` parameter is folded into the line as `#name`, so the
    /// shared capture parser resolves it rather than this file inventing a
    /// second way to name a stream.
    @Test
    func captureFoldsTheStreamIntoTheLine() {
        #expect(link("sunrise://capture?text=Buy%20milk&stream=Errands")
            == .capture(text: "Buy milk #Errands"))
        #expect(link("sunrise://capture?text=Buy%20milk") == .capture(text: "Buy milk"))
    }

    /// `#two words` is not something the parser can express, so the name is
    /// dropped and the text survives. Losing the whole capture would be worse.
    @Test
    func aStreamNameWithASpaceIsDroppedRatherThanMangled() {
        #expect(link("sunrise://capture?text=Call&stream=Big%20Client") == .capture(text: "Call"))
    }

    /// Every refusal. Wrong scheme, unknown route, an id that is not one, a
    /// kind mismatch, an action this build does not know, and an empty
    /// capture.
    @Test
    func unknownShapesAreIgnored() {
        let refused = [
            "https://morning",
            "sunrise://sunrise",
            "sunrise://morning/extra",
            "sunrise://task/not-an-id",
            "sunrise://task/\(block)",
            "sunrise://task/\(task)?action=detonate",
            "sunrise://entity/xxx_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "sunrise://entity/tsk_short",
            "sunrise://capture",
            // A block is not something to focus on, and a focus link with no
            // task is the sidebar's own destination written as a URL.
            "sunrise://focus/\(block)",
            "sunrise://focus",
            // Sharing is deferred from v1 by ADR-0020: there is no grant a
            // token could name, so the link is dropped rather than routed.
            "sunrise://share/abc123"
        ]
        for raw in refused {
            #expect(link(raw) == nil, "\(raw) should be ignored")
        }
    }

    /// The link a notification carries has to parse back into the same thing,
    /// or a tapped reminder opens the wrong screen.
    @Test
    func everyLinkSurvivesARoundTrip() {
        let all: [DeepLink] = [
            .morningSummary,
            .endOfDayPlan,
            .entity(block),
            .capture(text: "Buy milk #Errands"),
            .task(task, .open),
            .task(task, .complete),
            .task(task, .snooze(.oneHour)),
            .task(task, .snooze(.tomorrow)),
            .task(task, .snooze(.nextWeek)),
            .focus(task)
        ]
        for original in all {
            #expect(
                original.url.flatMap(DeepLink.init(url:)) == original,
                "\(original) did not survive"
            )
        }
    }
}

/// What a link *does* once it has parsed, against a real vault.
///
/// The suite above is pure and about refusals. This one is the other half of
/// issue #40: a link that names an entity has to arrive at the entity, and a
/// link that names a task to focus on has to start the session — neither of
/// which a parser test can see.
@MainActor
struct DeepLinkRoutingTests {
    private let missing = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV"
    private let block = "blk_01ARZ3NDEKTSV4RRFFQ69G5FAV"

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

    /// The request the shell takes: a screen, **and** the thing to show on it.
    @Test
    func anEntityLinkAsksForAScreenAndForTheEntityOnIt() {
        let surfaces = AppSurfaces()

        surfaces.open(.entity(block))

        #expect(surfaces.pendingDestination == .calendar)
        #expect(surfaces.pendingReveal == block)
        surfaces.revealTaken()
        #expect(surfaces.pendingReveal == nil)
    }

    @Test
    func anEntityLinkForATaskResolvesToTheTaskItself() async throws {
        let vault = try await TestVault()
        let models = VaultModels(bridge: vault.bridge)
        let created = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))

        let revealed = await models.reveal(created.entity)

        guard case let .task(item)? = revealed else {
            Issue.record("a task link must resolve to the task, got \(String(describing: revealed))")
            await vault.bridge.shutdown()
            return
        }
        #expect(item.id == created.entity)
        #expect(item.title == "Renew passport")
        await vault.bridge.shutdown()
    }

    /// The other kind, and the one a block reminder sends: the grid is where
    /// a block lives, and it opens on today, so the reveal is the move onto
    /// the block's own day.
    @Test
    func anEntityLinkForABlockMovesTheGridOntoIt() async throws {
        let vault = try await TestVault()
        let models = VaultModels(bridge: vault.bridge)
        let calendar = models.calendar
        await calendar.refresh()
        let start = calendar.windowStartMs + 9 * 3_600_000
        await calendar.createBlock(
            fromMs: start,
            toMs: start + 3_600_000,
            title: "Quarterly review",
            kind: .zoned
        )
        let id = try #require(calendar.rows.first?.block.id)
        await calendar.step(3)

        let revealed = await models.reveal(id)

        guard case let .block(row) = revealed else {
            Issue.record("a block link must resolve to the block")
            await vault.bridge.shutdown()
            return
        }
        #expect(row.block.id == id)
        #expect(calendar.placed(dayOffset: 0).map(\.id) == [id])
        await vault.bridge.shutdown()
    }

    /// "A link naming an entity that does not exist resolves to the nearest
    /// sensible screen rather than an error dialog" — `interaction-patterns.md`
    /// §URL scheme. The screen still opens; there is simply nothing on it to
    /// reveal, which is what a reminder tapped after its task was deleted on
    /// another device looks like.
    @Test
    func anEntityThisVaultDoesNotHoldRevealsNothing() async throws {
        let vault = try await TestVault()
        let models = VaultModels(bridge: vault.bridge)

        #expect(await models.reveal(missing) == nil)

        await vault.bridge.shutdown()
    }

    /// `sunrise://focus/<TaskId>` makes the write `F` on a row makes.
    @Test
    func aFocusLinkStartsTheSessionItNames() async throws {
        let vault = try await TestVault()
        let surfaces = AppSurfaces()
        surfaces.attach(bridge: vault.bridge)
        let created = try await vault.bridge.submit(.createTask(draft: draft("Write the report")))

        await surfaces.beginFocus(on: created.entity)

        let running = try await FocusSessions.running(in: vault.bridge)
        #expect(running?.start.taskId == created.entity)
        surfaces.detach()
        await vault.bridge.shutdown()
    }

    /// **And never a second one.** The core holds two open sessions happily;
    /// the Focus screen renders the newest, so a link that started one on top
    /// of a running session would take the timer away from whoever was
    /// watching it — and they clicked a link, so they were not.
    @Test
    func aFocusLinkWillNotStartASecondSession() async throws {
        let vault = try await TestVault()
        let surfaces = AppSurfaces()
        surfaces.attach(bridge: vault.bridge)
        let first = try await vault.bridge.submit(.createTask(draft: draft("The one in progress")))
        let second = try await vault.bridge.submit(.createTask(draft: draft("The one linked to")))

        await surfaces.beginFocus(on: first.entity)
        await surfaces.beginFocus(on: second.entity)

        let running = try await FocusSessions.running(in: vault.bridge)
        #expect(running?.start.taskId == first.entity)
        surfaces.detach()
        await vault.bridge.shutdown()
    }
}
