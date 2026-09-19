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

/// Detaching a consumer costs an actor hop, so there is no synchronous moment
/// to assert at. Poll rather than sleep a guessed amount.
private func waitUntil(
    timeout: Duration = .seconds(2),
    _ condition: @Sendable () async -> Bool
) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: timeout)
    while ContinuousClock.now < deadline {
        if await condition() { return true }
        try? await Task.sleep(for: .milliseconds(5))
    }
    return await condition()
}

/// Drain a finished stream.
private func drain(_ stream: AsyncStream<CoreChange>) async -> [CoreChange] {
    var seen: [CoreChange] = []
    for await change in stream { seen.append(change) }
    return seen
}

/// The feed has one producer and as many consumers as there are screens.
///
/// It did not always: the bridge kept a single subscription and cancelled the
/// previous one, so the last screen to appear owned the feed and every other
/// one silently stopped repainting. These are the tests that fail against that.
struct ChangeBroadcastTests {
    @Test
    func everyConsumerSeesEveryChange() async {
        let broadcast = ChangeBroadcast()
        let first = await broadcast.subscribe()
        let second = await broadcast.subscribe()
        let third = await broadcast.subscribe()

        await broadcast.publish(.entity("tsk_a"))
        await broadcast.publish(.entity("tsk_b"))
        await broadcast.finish()

        let expected: [CoreChange] = [.entity("tsk_a"), .entity("tsk_b")]
        #expect(await drain(first) == expected)
        #expect(await drain(second) == expected, "the second screen went dead")
        #expect(await drain(third) == expected, "the third screen went dead")
    }

    /// The exact failure the old bridge had, at the level that can be made
    /// deterministic: one consumer ending must not cost another its feed.
    @Test
    func endingOneConsumerLeavesTheOthersLive() async {
        let broadcast = ChangeBroadcast()
        let leaving = await broadcast.subscribe()
        let staying = await broadcast.subscribe()

        // A screen that appears and then goes away. Cancellation is how it
        // actually happens: SwiftUI cancels the `.task` a view started, which
        // ends the `for await` and terminates that consumer's stream.
        let departed = Task {
            for await _ in leaving {}
        }
        await broadcast.publish(.entity("tsk_a"))
        departed.cancel()
        await departed.value
        #expect(
            await waitUntil { await broadcast.consumerCount == 1 },
            "a departed consumer was never detached"
        )

        await broadcast.publish(.entity("tsk_b"))
        await broadcast.finish()
        #expect(await drain(staying) == [.entity("tsk_a"), .entity("tsk_b")])
    }

    /// A lag means "re-run every query you are showing". A consumer that is
    /// not told keeps rendering pre-burst rows while its neighbour repaints,
    /// which is the worst of both: stale *and* inconsistent.
    @Test
    func aLagReachesEveryLiveConsumerNotJustTheFirst() async {
        let broadcast = ChangeBroadcast()
        let window = Duration.milliseconds(10)
        let first = await broadcast.subscribe().coalesced(window: window)
        let second = await broadcast.subscribe().coalesced(window: window)

        await broadcast.publish(.entity("tsk_a"))
        await broadcast.publish(.lagged(skipped: 4743))
        await broadcast.finish()

        var firstBatches: [ChangeBatch] = []
        for await batch in first { firstBatches.append(batch) }
        var secondBatches: [ChangeBatch] = []
        for await batch in second { secondBatches.append(batch) }

        #expect(firstBatches.contains { !$0.isComplete })
        #expect(
            secondBatches.contains { !$0.isComplete },
            "the second consumer was never told it had lost notifications"
        )
    }

    @Test
    func aClosedVaultEndsEveryConsumer() async {
        let broadcast = ChangeBroadcast()
        let first = await broadcast.subscribe()
        let second = await broadcast.subscribe()

        await broadcast.publish(.closed)

        #expect(await drain(first) == [.closed])
        #expect(await drain(second) == [.closed])
        #expect(await broadcast.consumerCount == 0)
    }

    /// A screen that appears after the vault closed is told so, rather than
    /// left awaiting a stream nothing will ever feed.
    @Test
    func subscribingAfterCloseIsAnsweredRatherThanIgnored() async {
        let broadcast = ChangeBroadcast()
        await broadcast.finish()
        #expect(await drain(broadcast.subscribe()) == [.closed])
    }

    /// **Subscribing and asking whether the feed was open is one hop.**
    ///
    /// `CoreBridge.changes()` needs both to decide whether a prime would be an
    /// instruction to re-read a core that refuses reads. Taking them as two
    /// awaits left a window: a `finish()` processed between them returned
    /// `false` for a feed that had already closed, so the prime went out and
    /// every `follow()` loop painted `CoreError.Closed`. The sticky-flag
    /// argument does not cover it — stickiness orders the *batches*, and what
    /// goes wrong is when the consumer's query runs.
    ///
    /// What is assertable without a scheduler hook is the contract: the flag
    /// and the stream agree, on both sides of `finish()`, and they are handed
    /// back together. Drop the `wasOpen` half and `CoreBridge` has to read the
    /// flag separately again, which is the shape that raced.
    @Test
    func subscribeIfOpenAnswersBothHalvesTogether() async {
        let live = ChangeBroadcast()
        let (openStream, wasOpen) = await live.subscribeIfOpen()
        #expect(wasOpen, "the feed had not closed, so a prime is honest")
        await live.finish()
        #expect(await drain(openStream) == [.closed])

        let closed = ChangeBroadcast()
        await closed.finish()
        let (closedStream, stillOpen) = await closed.subscribeIfOpen()
        #expect(
            !stillOpen,
            """
            the feed had closed, and a prime would tell every screen to re-read \
            a core that refuses reads
            """
        )
        #expect(await drain(closedStream) == [.closed])
    }
}

/// `primed()` is what makes a lazily-opened subscription safe to open late.
struct PrimedChangeStreamTests {
    /// The prime arrives with nothing behind it, so a screen re-reads before
    /// anything has happened rather than after the first thing it missed.
    @Test
    func aPrimedStreamOpensWithARepaintNobodyPublished() async {
        let broadcast = ChangeBroadcast()
        let stream = await broadcast.subscribe()
            .coalesced(window: .milliseconds(5))
            .primed()
        var iterator = stream.makeAsyncIterator()

        let prime = await iterator.next()
        #expect(prime?.touched.isEmpty == true)
        #expect(
            prime?.isComplete == false,
            "an empty batch that claimed completeness would say 'nothing changed'"
        )
        #expect(prime?.isClosed == false)
        await broadcast.finish()
    }

    /// And it is a prefix, not a replacement: what is published afterwards
    /// still arrives behind it.
    ///
    /// **No sleeps, and none are needed.** `primed()` yields the prime as its
    /// first act and only *then* begins consuming upstream, so nothing can
    /// overtake it however the tasks are scheduled. Every stream in the chain
    /// buffers unbounded — `subscribe()` says so outright, `coalesced()` and
    /// `primed()` take `AsyncStream`'s default — so a batch yielded before the
    /// collector starts iterating is held rather than dropped, and `finish()`
    /// delivers what is buffered before it ends the stream. `coalesced()` in
    /// turn refuses to let upstream ending swallow an open window — it flushes
    /// the batch before finishing its own stream — which is what carries
    /// `tsk_a` past the `finish()` below. Both assertions therefore hold on
    /// every interleaving, and a sleep would only be a flake waiting for a
    /// loaded runner.
    ///
    /// Ordering *between* post-prime batches is not claimed here, because one
    /// published item cannot pin it and separating two into two batches needs a
    /// pause longer than the coalescing window — a sleep again.
    /// ``ChangeCoalescingTests/aPauseEndsTheWindow()`` is where that is pinned.
    @Test
    func thePrimeDoesNotSwallowWhatFollows() async {
        let broadcast = ChangeBroadcast()
        let stream = await broadcast.subscribe()
            .coalesced(window: .milliseconds(5))
            .primed()

        let collected = Task { () -> [ChangeBatch] in
            var batches: [ChangeBatch] = []
            for await batch in stream { batches.append(batch) }
            return batches
        }
        await broadcast.publish(.entity("tsk_a"))
        await broadcast.finish()

        let batches = await collected.value
        #expect(batches.first?.touched.isEmpty == true, "the prime comes first")
        #expect(batches.contains { $0.touched == ["tsk_a"] })
    }
}
