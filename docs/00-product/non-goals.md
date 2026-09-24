---
status: accepted
---

# Non-Goals

Saying "no" is the most important product work. Each non-goal here was tempting at some point and was rejected for a specific reason.

## Not a wiki / second brain

Sunrise stores **notes attached to tasks/streams**, not a free-form knowledge graph. No backlinks, no transclusion, no graph view. Reason: every "second brain" tool collapses under maintenance burden, and the audience that wants this is well served elsewhere (Obsidian, Logseq).

## Not a chat / collaboration tool

No messaging, comments threads, mentions-with-notifications, or activity feeds. Sharing is *granting access to view/edit a stream or task*, not a conversation surface. Reason: chat collapses the encryption design (real-time presence + indexable search) and pulls scope toward team productivity.

## Not a CRM

No deals, pipelines, or per-contact histories. People entities exist only as **shareable identities** and references in task assignment. Reason: CRM is a vertical product.

## Not a calendar

Sunrise *integrates* with calendars and supports time-blocking on top of them. It does not replace your calendar. Reason: calendaring (invites, RSVP, free/busy across orgs, calendar-aware OAuth) is a 10-year product on its own.

## Not a habit tracker (as the primary axis)

Habits are modeled as routines — recurring tasks with streak tracking — but the app is not "open Sunrise to log your habits." Reason: habit-as-primary apps lose users who fall off; productivity-with-routines retains them.

## Not a team/enterprise product

No org admin, SSO, audit logs for compliance, role hierarchies. **Self-hosting is the answer for teams that want shared infrastructure.** Reason: enterprise sales kills the product surface area we care about.

## Not an AI agent platform

We may include narrow, well-scoped AI affordances (smart capture parsing, weekly review summarization, scheduling suggestions). We will not build a general agent that "manages your tasks for you." Reason: agentic features that promise more than they deliver erode trust permanently.

## Not a cross-platform sync as a service

The sync server is part of Sunrise, not a sellable product to other apps. Reason: focus.

## Not a Markdown editor

Notes use a constrained rich text format (see [`02-domain/notes.md`](../02-domain/notes.md)). Reason: a Markdown editor invites comparison to all Markdown editors, which is a fight not worth having.

## Not an automation platform

No if-this-then-that rules, no scripting, no cron-driven actions, no outbound webhooks. Reason: every automation system grows unbounded — triggers, conditions, actions, debugging, dry-run, loop prevention, rate limits — and most of the value is captured by recurring routines (which we *do* ship). Users who need automation can drive Sunrise from outside via the OS automation surfaces already exposed (App Intents on Apple, Tasker / App Actions on Android, the CLI on desktop).

## Not an inbound email gateway

No `you@in.sunrise.example` capture address. Reason: inbound email is operationally heavy (DKIM/SPF/DMARC, abuse handling, attachment storage) and breaks the E2EE story (the receiving server sees plaintext). The native share sheets on iOS / Android and a browser extension cover the "save this for later" use case.

## Not a calendar protocol server, and not a calendar editor

We **read** Google Calendar, Microsoft 365 / Outlook / Exchange (through
Microsoft Graph) and CalDAV servers (iCloud, Fastmail, Nextcloud, …), and we
import and export iCalendar (.ics) files
([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)). We do
not serve CalDAV, and we do not write to anyone's calendar: no creating,
editing, moving or responding to external events. Reason: a calendar protocol
server is its own product, and write access to someone's calendar is a much
larger trust request and a conflict-resolution problem per provider. Sunrise
plans *around* the calendar; the calendar stays the source of truth for
meetings.

## Not a custom auth stack

We do not implement password storage, email-OTP delivery, magic links, MFA, captcha, or our own session/token format. All login, sign-up, MFA, password reset, and email verification go through an **OIDC** issuer (Sunrise-operated for managed cloud; operator-chosen for self-host). Reason: every one of those is a permanent surface for security bugs and engineering hours; OIDC issuers do all of it well. See [`../06-server/auth.md`](../06-server/auth.md).

## No feature flags / per-user gates

The product is opinionated. Every shipped feature is on for every user on every supported platform; there is no flag service, no LaunchDarkly-style gating, no per-user rollout controls. Reason: feature gates explode test surface, mask broken code paths, and signal a lack of conviction. We turn features on by shipping them.

## No LAN-only / no-server topology

Pairing and sync always go through a server (managed or self-hosted). Reason: an extra "no-server" code path doubles the transport-layer test matrix while serving a vanishingly small audience. Self-hosters who want LAN-only operation run the server on the LAN.

---

**Test.** When a feature request arrives, the first question is: "does this push us toward a non-goal above?" If yes, default to no.
