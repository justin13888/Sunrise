# 0072 — Delight is functional motion on the shared motion tokens; a boredom screen and stress-relief tools are non-goals

**Status:** accepted

**Amends** [`../00-product/non-goals.md`](../00-product/non-goals.md): it adds
"Not a game or a wellness app".

**Amends** [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md)
§What is not shared. "Animation choices beyond motion tokens" stays
per-platform: which element moves, and whether it slides, fades or scales, is
each platform's idiom. What that page left open is now settled: the duration
and curve of any animation a view writes come from the motion tokens, never a
literal (decision 1).

**Depends on** [ADR-0029](./0029-design-token-pipeline.md) (the token pipeline
that emits `tokens.swift`) and
[`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
§Motion.

**Answers** [#13](https://github.com/justin13888/Sunrise/issues/13), which
asked whether three things belong in Sunrise: (a) a boredom screen, (b)
stress-relief tools, and (c) in-app animations that guide attention and
motivate task completion.

## Context

Issue #13 was filed as an exploration. It brushes two settled positions:
[`interaction-patterns.md`](../07-clients/interaction-patterns.md) §Mark done
("Confetti? No. We don't gamify.") and
[`focus-mode.md`](../08-features/focus-mode.md) §What we don't do ("We don't
gamify focus"). It also carried an open fork that a later review of the issue
named: does motion target SwiftUI in `apps/apple`, using its own animation
primitives and `accessibilityReduceMotion`, or the TypeScript motion-token
layer, which at the time had no path into Swift?

The tree has since closed that fork:

- `packages/sunrise-ui-tokens/tokens/motion.toml` is compiled into
  `generated/tokens.swift`, `tokens.css`, `tokens.ts` and `tokens.rs`
  (ADR-0029), and both Apple app targets compile `tokens.swift`.
- `apps/apple/Sunrise/Design/Tokens.swift` defines `Motion.fast`,
  `Motion.standard` and `Motion.slow`. Each builds its animation from a
  generated token and returns `nil` when `\.accessibilityReduceMotion` is set.
- `tokens.css` carries the matching `prefers-reduced-motion: reduce` block.
- `DropHighlight` already animates through `Motion.fast`. The one other
  animation in the app, the command palette's scroll in
  `CommandPaletteView.swift`, uses a literal `.linear(duration: 0.1)` that
  ignores Reduce Motion.

So the choice is no longer between SwiftUI and the tokens: SwiftUI reads the
tokens. What is left to decide is which of (a), (b) and (c) Sunrise builds at
all.

## Decision

1. **Motion targets the shared tokens, on every client.** A view animates only
   through its platform's adapter over `motion.toml`. On Apple platforms that
   adapter is `Motion` in `Sunrise/Design/Tokens.swift`. On the web it is the
   generated CSS custom properties. A view never writes a literal duration,
   curve or spring. Reduce Motion is honoured in the adapter, once, and not at
   each call site. The kind of motion stays each platform's idiom
   ([`shared-ui-system.md`](../07-clients/shared-ui-system.md) §What is not
   shared): a view chooses whether something slides, fades or scales, and a
   transition the platform draws itself, such as a navigation push, is not a
   literal in a view. Only the timing is shared. Apple is the first host
   because it is the only client whose views animate today. The web client in
   `apps/web` already imports `@sunrise/ui-tokens/css`, so the motion custom
   properties and their `prefers-reduced-motion` block are in its bundle. Any
   motion it adds, including in its calendar
   ([#944](https://github.com/justin13888/Sunrise/issues/944), P6), reads
   those same tokens, so the two clients never diverge.
2. **(a) A boredom screen is out of scope.** Sunrise ships no mini-game, idle
   toy or play surface. This is a non-goal, recorded in `non-goals.md`.
3. **(b) Stress-relief tools are out of scope.** Sunrise ships no breathing
   exercise, fidget surface, calming screen or mood check. This is a non-goal,
   recorded in `non-goals.md`.
4. **(c) Animation is in scope only as feedback.** Motion shows the user a
   state change they caused, or one they need to notice:
   - a task completing,
   - a drag, its drop target and its drop,
   - a surface arriving or leaving,
   - a list reordering.

   Six rules bound it:
   - **The state change is not held up by its animation.** The op is written
     when the user acts, and the next input is accepted at once. Motion only
     presents a change that has already happened.
   - **No reward.** No confetti, celebration, sound, points, badge, or
     animation that grows with a count or a streak. Completing a task looks
     the same on the first day as on the hundredth.
   - **No motion without a cause.** No looping or idle animation, and no
     ambient motion that competes for attention. A cue that draws the eye,
     such as a block that is about to start, moves once and then stops.
   - **Motion never carries information on its own**
     ([`accessibility.md`](../10-cross-cutting/accessibility.md) §Motion). The
     end state says everything the motion said.
   - **Durations come only from the token scale:** `fast` for a change the
     user is looking at, `standard` for something that moves, and `slow` for a
     whole surface. Under Reduce Motion every one is zero.
   - **Motion is not a setting.** Reduce Motion is the system's switch, and
     Sunrise adds no second one
     ([`non-goals.md`](../00-product/non-goals.md) §No feature flags).
5. **#13 closes with this record.** The feature work for (c) is tracked as its
   own issue, [#1087](https://github.com/justin13888/Sunrise/issues/1087),
   ranked in P3 of [`../roadmap.md`](../roadmap.md). It is deferred
   past the stabilization pass for the same reason as #13: it adds product
   surface, and stabilization adds none.

## Alternatives considered

- **SwiftUI's own animation primitives, with no token bridge.** This was the
  fork #13 named. The bridge it assumed was missing now exists, so taking this
  path would throw away a built, drift-checked pipeline. It would also put
  Reduce Motion handling at every call site. The command palette's literal
  scroll animation shows the cost: it has no Reduce Motion handling.
- **Build the TypeScript motion layer first, then port it.** Rejected. The web
  client in `apps/web` has no animated views yet: its calendar
  ([#944](https://github.com/justin13888/Sunrise/issues/944)) is P6, and
  `packages/sunrise-ui` re-exports the tokens with no components. Waiting for
  web motion would hold Apple motion behind work that is not scheduled. The tokens are already the shared layer, so nothing would be
  gained by waiting.
- **Ship (a) or (b) as an opt-in, peripheral feature.** Rejected. The issue
  proposed this as its mitigation, but [`non-goals.md`](../00-product/non-goals.md)
  §No feature flags rules out a feature some users turn on. An optional
  surface would be built, tested and translated for everyone, and would
  re-frame a planning tool as a place to spend time. Users who want these
  tools are well served by dedicated apps.
- **Motivational animation: celebrate completion, animate streaks.**
  Rejected for the reasons in `interaction-patterns.md` and `focus-mode.md`.
  A reward schedule turns completing a task into a game score, and a missed
  day becomes a loss the app staged. Routine streaks stay a number shown
  plainly ([`routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Streak), and are not animated.

## Consequences

- `non-goals.md` gains "Not a game or a wellness app". A request for a
  mini-game, breathing timer or calming screen is answered by pointing to it.
- Each animation added to a client is reviewable against the rules in decision
  4. A literal duration or curve in a view is a defect.
- The command palette's literal scroll animation is now out of line with
  decision 1, and is fixed by #1087.
- Web motion needs no design work of its own: its durations and the
  reduced-motion block are already generated.

## Revisit triggers

1. User research shows that feedback motion alone fails to make completion
   clear. That would reopen decision 4's "no reward" rule, and nothing else.
2. A platform's adapter cannot express a needed effect, such as a spring that
   carries a gesture's velocity, from the token scale. The answer is a new
   token in `motion.toml`, not a literal in a view.
