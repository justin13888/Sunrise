---
status: draft
---

# Reviews and Stats

Periodic review is the most important habit Sunrise wants to enable. Stats support reviews; they are not a leaderboard.

## Weekly review

Triggered by the user's chosen cadence (default Friday afternoon, configurable). The review is a guided flow:

### Step 1 — Per-Stream summary

For each non-archived, non-paused Stream:

- Tasks completed this week: count + list (collapsed).
- Tasks deferred ≥ 1 time: list (full).
- Tasks created this week and not yet acted on: list.
- Recent notes added.

### Step 2 — Inbox triage

If the inbox has untriaged items, the review enters one-at-a-time triage: keep / promote / defer / drop.

### Step 3 — Routines tuning

For each Routine with non-trivial drift (skipped > N% in the last K weeks):

- Suggest pausing.
- Suggest changing cadence.
- Show streak.

### Step 4 — Slipped commitments

Tasks that were due/scheduled this week and remain incomplete:

- Per task: do today / defer to next week / drop / convert to routine.

### Step 5 — Output

A short summary screen:

- Counts: completed / deferred / dropped.
- Streaks updated.
- Saved review snapshot stored as an opaque entity (queryable in History).

The review takes 5–15 minutes for a typical multi-stream user. We don't try to make it shorter at the cost of meaningful reflection.

## Daily review (optional)

A lightweight 60-second flow:

- Inbox glance: any captures from yesterday-evening to clear?
- Today preview: the day's planned tasks.
- "Any blocker for today?" prompt.

Off by default.

## Stats

Per Stream:

- Completed-per-week trend (last 12 weeks).
- Deferred-per-week trend.
- Routine streaks.
- Time-in-focus (when known) per Stream.

We do not surface:

- Productivity scores.
- Cross-user comparisons.
- "Hours to clear backlog" projections (anti-pattern).

## Activity timeline (per entity)

Each Task / Stream has an activity feed: created, edited, completed, deferred, focus-session, etc. Useful for "what happened?" not for analytics.

## Export

Stats are computable from the op log; we export them on request as JSON or CSV via the export pipeline.

## Privacy

Stats are computed locally. Aggregated stats are *not* sent to the server. A user might opt in to anonymized usage telemetry (off by default, see [`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md)).
