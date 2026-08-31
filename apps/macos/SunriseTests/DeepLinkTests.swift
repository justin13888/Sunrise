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

    /// A block belongs on the grid, a task on Today. Coarser than the doc's
    /// "open entity in detail", but real — and it is what a block reminder's
    /// body tap needs.
    @Test
    func anEntityLinkLandsWhereThatKindLives() {
        #expect(link("sunrise://entity/\(block)")?.destination == .calendar)
        #expect(link("sunrise://entity/\(task)")?.destination == .list(.todayAll))
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
            "sunrise://focus/\(task)",
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
            .task(task, .snooze(.nextWeek))
        ]
        for original in all {
            #expect(
                original.url.flatMap(DeepLink.init(url:)) == original,
                "\(original) did not survive"
            )
        }
    }
}
