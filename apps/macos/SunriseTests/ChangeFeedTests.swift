import Foundation
import Testing

@testable import Sunrise

/// The change feed is the one place a correct-looking app quietly goes wrong,
/// so these tests are about loss, not about delivery.
struct ChangeAccumulatorTests {
    @Test
    func aLagInvalidatesTheIdListRatherThanAddingToIt() {
        var accumulator = ChangeAccumulator()
        accumulator.record(.entity("tsk_a"))
        accumulator.record(.lagged(skipped: 4743))
        accumulator.record(.entity("tsk_b"))

        let batch = accumulator.drain()
        // Both ids are still true — they are just no longer all of the truth.
        #expect(batch?.touched == ["tsk_a", "tsk_b"])
        #expect(batch?.isComplete == false)
    }

    @Test
    func anUninterruptedBurstStaysComplete() {
        var accumulator = ChangeAccumulator()
        accumulator.record(.entity("tsk_a"))
        accumulator.record(.entity("tsk_a"))
        accumulator.record(.entity("tsk_b"))

        let batch = accumulator.drain()
        #expect(batch?.touched == ["tsk_a", "tsk_b"])
        #expect(batch?.isComplete == true)
    }

    @Test
    func drainingTwiceDoesNotRepaintTwice() {
        var accumulator = ChangeAccumulator()
        accumulator.record(.entity("tsk_a"))
        #expect(accumulator.drain() != nil)
        #expect(accumulator.drain() == nil)
    }

    /// A completeness flag that reset itself would be worse than no flag: the
    /// next batch would look authoritative while missing whatever was lost.
    @Test
    func completenessIsPerBatchButClosureIsNot() {
        var accumulator = ChangeAccumulator()
        accumulator.record(.lagged(skipped: 1))
        #expect(accumulator.drain()?.isComplete == false)

        accumulator.record(.entity("tsk_a"))
        #expect(accumulator.drain()?.isComplete == true)

        accumulator.record(.closed)
        #expect(accumulator.drain()?.isClosed == true)
        #expect(accumulator.drain()?.isClosed == true, "a closed vault does not re-open")
    }
}

struct ChangeCoalescingTests {
    /// The case the bridge exists for: a sync catch-up dumping thousands of
    /// notifications must cost one repaint, and that repaint must know it is
    /// looking at an incomplete id list.
    @Test
    func aCatchUpBurstBecomesOneIncompleteRepaint() async {
        let (upstream, continuation) = AsyncStream<CoreChange>.makeStream(bufferingPolicy: .unbounded)
        for index in 0..<5000 {
            continuation.yield(.entity("tsk_\(index)"))
        }
        continuation.yield(.lagged(skipped: 4743))
        continuation.finish()

        var batches: [ChangeBatch] = []
        for await batch in upstream.coalesced(window: .milliseconds(50)) {
            batches.append(batch)
        }

        #expect(batches.count == 1, "a burst repainted \(batches.count) times")
        #expect(batches.first?.touched.count == 5000)
        #expect(batches.first?.isComplete == false)
    }

    /// Two bursts separated by more than a window are two repaints; collapsing
    /// them would mean an edit made after a pause waits on the next one.
    @Test
    func aPauseEndsTheWindow() async {
        let (upstream, continuation) = AsyncStream<CoreChange>.makeStream(bufferingPolicy: .unbounded)
        let window = Duration.milliseconds(20)

        let collector = Task {
            var batches: [ChangeBatch] = []
            for await batch in upstream.coalesced(window: window) {
                batches.append(batch)
            }
            return batches
        }

        continuation.yield(.entity("tsk_a"))
        try? await Task.sleep(for: .milliseconds(120))
        continuation.yield(.entity("tsk_b"))
        try? await Task.sleep(for: .milliseconds(120))
        continuation.finish()

        let batches = await collector.value
        #expect(batches.count == 2, "expected two repaints, got \(batches.count)")
        #expect(batches.first?.touched == ["tsk_a"])
        #expect(batches.last?.touched == ["tsk_b"])
    }

    /// Nothing in, nothing out. An empty repaint would still cost a full
    /// re-query on the far side.
    @Test
    func silenceProducesNoRepaint() async {
        let (upstream, continuation) = AsyncStream<CoreChange>.makeStream()
        continuation.finish()

        var count = 0
        for await _ in upstream.coalesced(window: .milliseconds(10)) { count += 1 }
        #expect(count == 0)
    }

    /// Every `ChangeEvent` case is a prompt to re-read the entity it names;
    /// none of them carries a payload, so all four collapse to one.
    @Test
    func everyEventKindNamesItsEntity() {
        #expect(CoreChange(.created(entity: "tsk_a")) == .entity("tsk_a"))
        #expect(CoreChange(.updated(entity: "tsk_a")) == .entity("tsk_a"))
        #expect(CoreChange(.deleted(entity: "tsk_a")) == .entity("tsk_a"))
        #expect(CoreChange(.forgotten(entity: "tsk_a")) == .entity("tsk_a"))
    }
}
