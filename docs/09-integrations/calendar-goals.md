---
status: accepted
---

# Calendar goals

**Sunrise's calendar read and write must beat dedicated calendar apps on features and
UI quality.** Sunrise is opened throughout the day, so someone using it should have no
reason to keep another calendar app open.

[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md) is the decision.
[`calendar-architecture.md`](./calendar-architecture.md) is the design and
[`calendar-competitors.md`](./calendar-competitors.md) is the evidence behind this list.

Nothing here is built yet. This page pins what done means.

## What "beats" means

A goal below is met only when both hold:

1. **It works against real provider accounts,** in the conformance suite for that
   provider tier.
2. **It holds up against the reference apps by hand.** Published comparisons do not
   cover recurrence editing, undo, search or drag quality for any app, so the benchmark
   ([#926](https://github.com/justin13888/Sunrise/issues/926)) has to establish the bar first.

Claims that Sunrise beats an app are made only from that benchmark.

## Providers

| Tier | Providers | Promise |
|---|---|---|
| Full | Google Calendar, Microsoft Graph (Outlook.com, Microsoft 365, Exchange Online) | Read, write, recurring edits, RSVP |
| Full, where the server allows | iCloud, Fastmail, Nextcloud over CalDAV | Read, write, recurring edits. RSVP where the server supports auto-scheduling |
| Best effort | Any other RFC 4791 server | Read and write. Sunrise names what the server lacks instead of failing silently |
| Read-only | ICS subscriptions | Periodic fetch of public calendars |

Each named provider is claimed only once it passes the suite. Generic "CalDAV support" is
never claimed.

## Baseline: what every reference app is measured on

**Input**
- Natural language entry with a visible parse preview before anything is created.
- A keyboard-first command bar that reaches every calendar action.

**Editing**
- Drag to create, move and resize, with a live preview.
- This, this and following, and all edits on a recurring event, with a preview of what
  changes before it is sent.
- Undo for every edit, including a pushed one.
- Offline edits that reach the provider later, on any device with a write grant.

**Time**
- A multi-zone gutter.
- Floating and all-day events that stay correct across travel.
- Travel time and leave alerts.

**People**
- Attendees and RSVP, showing plainly that the provider sends the invitation.
- Other people's availability overlaid where the provider allows it.
- Video meeting links: join and add.

**Views**
- Day, week, month and schedule, with density options.
- Saved calendar sets.
- Full search across events.

**Surfaces**
- Menu bar, widgets, lock screen, Live Activities and watch where the platform offers them.

**Trust**
- A sync health screen that says which account failed, when it last succeeded, and whether
  the fault is Sunrise's or the provider's.
- Duplicate review instead of silent duplicates.
- Conflicts shown side by side and resolved by the user. No silent overwrite.

## Differentiators: what dedicated apps cannot do

Sunrise already holds tasks, routines, blocks and focus sessions, which a calendar app does
not. Whether each is truly unfilled is an inference until the benchmark checks it.

1. **One object, one lifecycle.** A task becomes a block, then a timer, then an actual, with
   no integration between them.
2. **Estimates that learn.** Actual time feeds future durations.
3. **Routines that reflow around meetings,** with no clutter on shared calendars.
4. **Explainable planning.** Every planner move shows why, previews first and undoes in one
   step ([ADR-0048](../11-adr/0048-interactive-planner.md)). Nothing moves without a gesture.
5. **A private planning layer.** A block stays in the encrypted vault, or publishes as busy,
   or publishes in full, per calendar.
6. **Full provider breadth and planning together.** Apps tend to offer one or the other.
7. **Always-open "now and next".** The current block, timer, next meeting and leave time in
   one place.

## Principles

1. **Local first.** Every read and edit works offline.
2. **The server sees no plaintext.** Credentials never leave the device that minted them.
3. **Never lose or duplicate a user's event.** On any doubt, keep both and ask.
4. **Failures are visible.** Never a silent log line.
5. **Private by default.** A Block reaches a provider only on an explicit choice.
6. **Core logic in Rust and deterministic,** tested in isolation. Only the browser step of
   OAuth, the keychain and the views are platform code.

## Out of scope

| Not doing | Why |
|---|---|
| Serving CalDAV | A protocol server is its own product |
| Server-rendered scheduling links | The server would see availability in plaintext. A sealed form may follow under its own ADR |
| Server-side fetching or push by default | The server would hold credentials or learn when calendars change. An opt-in doorbell may follow under its own ADR |
| Inbound invitation email (iMIP) | Inbound email stays a non-goal |
| Org admin and room management | Sunrise is not a team or enterprise product |
| An autonomous scheduler | Planner moves follow a gesture |
| JMAP Calendars, for now | Its RFCs are unpublished and no major provider offers it that we know of |
| EventKit, for now | Stays out unless the iCloud conformance work forces the question |

## Platform order

Apple clients and the CLI come first, then the web client as a token-less editor. Android and
Windows have no client today, and calendar arrives on them with their client. The design keeps
them possible: the logic is in Rust and each platform adds only its OAuth step, keychain and
views.
