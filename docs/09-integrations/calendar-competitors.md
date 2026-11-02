---
status: living
---

# Calendar competitor analysis

Reference apps for [`calendar-goals.md`](./calendar-goals.md), as of 2 November 2026. Scope: the best
ad-free calendar apps on macOS, Windows, Android and iOS, free or paid.

**How far to trust this.** Status, price and account data come mostly from vendor pages. Independent evidence
is thin, and Reddit and Hacker News were effectively unreachable. **Recurrence editing, undo, search and drag
behaviour are unverified for every app.** Check prices before quoting them. Labels: **[snippet]** is search
result text only, **[vendor]** is a self-claim, **[anecdotal]** is user opinion, and **[memory]** is unverified
recall. Everything else links to a page that was opened.

## Best per platform

These are judgment from the evidence, not a tested ranking.

| Platform | Primary reference | Secondary references |
|---|---|---|
| macOS | Fantastical (polish, input), BusyCal (customization, offline, price) | Notion Calendar (keyboard), Vimcal (scheduling) |
| iOS | Fantastical | Structured (daily timeline, routines), Vimcal free tier |
| Windows | Morgen, Notion Calendar | Fantastical for Windows (parity unverified) |
| Android | Notion Calendar, Morgen | Fossify + DAVx5 for privacy and CalDAV |

Android and Windows have no independent 2026 quality comparison in this evidence.

## Status and ads

No shutdown or acquisition in 2026 was found among these, though several were only lightly checked.

| App | Status | Ads |
|---|---|---|
| [Fantastical](https://flexibits.com/pricing?product=fantastical) | Alive; [15th anniversary, May 2026](https://mjtsai.com/blog/2026/05/19/fantastical-at-15/) | None seen |
| [BusyCal](https://busymac.com/busycal/) | Alive | None mentioned |
| Apple Calendar | Bundled | None seen |
| [Notion Calendar](https://www.notion.com/product/calendar) (formerly Cron) | Alive | None mentioned |
| [Morgen](https://www.morgen.so/pricing) | Alive | Paid only |
| [Amie](https://amie.so/pricing) | Alive, now calendar plus AI meeting notes | Paid plans listed |
| [Akiflow](https://akiflow.com/pricing) | Alive | Paid only |
| [Motion](https://www.usemotion.com/pricing) | Alive, now an AI work suite | Paid |
| [Reclaim](https://reclaim.ai/pricing) | [Acquired by Dropbox, 2024](https://reclaim.ai/blog/dropbox-acquires-reclaim), sold separately | None mentioned |
| [Sunsama](https://www.sunsama.com/pricing) | Alive | Paid only |
| [Vimcal](https://www.vimcal.com/pricing) | Alive | None mentioned |
| [Proton Calendar](https://proton.me/support/proton-calendar-guide) | Alive | None mentioned |
| [Fossify Calendar](https://github.com/FossifyOrg/Calendar/releases) | Active | None seen |
| [Etar](https://github.com/Etar-Group/Etar-Calendar/releases) | Possibly slow; release year not shown | None seen |
| [Structured](https://apps.apple.com/us/app/structured-daily-planner/id1499198946) | Alive | None mentioned |
| DAVx5 | A sync adapter, not a calendar app | None seen |
| **TimeTree** | Alive | **Free tier has ads: excluded** ([Capterra users](https://www.capterra.com/p/246790/TimeTree/reviews/)) [snippet] |
| **Outlook** | Alive | **Free version has ads: excluded** ([TechRadar](https://www.techradar.com/best/best-calendar-apps)) [snippet]. Paid Microsoft 365 is unverified |
| Google Calendar | Alive | **Unconfirmed.** Recollection is no display ads [memory]. Treat as a reference pending a device check |
| [Readdle Calendars](https://apps.apple.com/us/app/calendars-planner-organizer/id608834326) | Alive | **Unconfirmed**; listing reports tracking identifiers |
| One Calendar | [Windows build on FileHippo](https://filehippo.com/download_one-calendar/) [snippet] | **Unconfirmed** |

## Platforms, price and accounts

"?" means not stated on the checked pages, not absent. Prices are per month unless stated.

| App | Platforms | Price | Google | Microsoft | iCloud | CalDAV |
|---|---|---|---|---|---|---|
| Fantastical | Mac, iOS, Watch, Vision, Windows | Free tier; premium reports conflict, from [$4.75 annual](https://zapier.com/blog/best-calendar-apps/) to [$6.99](https://timingapp.com/blog/best-calendar-apps-for-mac/) [snippet] | Yes | Yes | Yes | Yes |
| BusyCal | Mac, iOS | $49.99 or Setapp | ? | ? | ? | ? |
| Notion Calendar | Mac, Windows, iPhone, Android | Free | Yes | Yes [snippet] | Yes | ? |
| Morgen | Windows, Mac, Linux, iOS, Android, web | $30, or $15 annual | Yes | Yes | Yes | [Fastmail full, others best effort](https://www.morgen.so/guides/caldavs-supported-by-v4) |
| Amie | Menu bar; others unconfirmed | Hidden | Yes | ? | Yes | ? |
| Akiflow | Web, desktop, mobile | $34, or $19 annual | Yes | Yes | ? | ? |
| Motion | iOS, Android, desktop | $19–$29 per seat | ? | ? | ? | ? |
| Reclaim | Web | Free tier; $10–$22 | Yes | Yes | ? | ? |
| Sunsama | iOS, Android, Mac, Windows, Linux | $17 annual, $22 monthly | Yes | Yes | ? | ? |
| Vimcal | Web, Mac, PC, iOS | Free on iOS; $25–$85 | Yes | Yes | ? | ? |
| Proton | Web, Android, iOS | Free 3 calendars; paid 25 | Import | Import | ? | ? |
| Structured | Apple confirmed | Free; Pro $2.99/mo to $99.99 lifetime | Pro; providers unnamed | | | |
| Fossify + DAVx5 | Android | DAVx5 free on F-Droid, paid on Play | Through the system store | ? | Yes | Yes |
| Readdle Calendars | iOS | $3.99, or $19.99/yr | Yes | ? | Yes | ? |

Morgen's split between full and best-effort CalDAV suggests generic CalDAV is costly to make reliable, which is
why [`calendar-goals.md`](./calendar-goals.md) names provider tiers.

## Features that separate best from average

**Y** means stated on a checked page, and **?** means not established.

| Feature | Fantastical | BusyCal | Notion | Morgen | Amie | Akiflow | Motion | Reclaim | Sunsama | Vimcal | Structured |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Natural language entry | Y | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Multiple time zones | ? | Y | Y | ? | ? | ? | ? | ? | ? | Y | ? |
| Scheduling links | Y | Beta | Y | Y | ? | Sharing | ? | Y | ? | Y | ? |
| Polls or proposals | Y | ? | ? | ? | ? | ? | ? | ? | ? | Y | ? |
| Others' availability | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Tasks on calendar | Y | Y | Notion items | Y | Y | Y | Y | Y | Y | ? | Y |
| Auto-scheduling | ? | ? | ? | Y | Y | Assistant | Y | Y | ? | ? | Replan |
| Command menu or shortcuts | ? | ? | Y | ? | ? | ? | ? | ? | ? | ? | ? |
| Meeting link | Y | ? | Y | ? | ? | ? | ? | Y | ? | ? | ? |
| Travel time | Y | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Menu bar | Y | Y | Y | ? | Y | ? | ? | ? | ? | Y | ? |
| Watch | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? | Y |
| Widgets or Live Activities | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? | Y |
| Calendar sets | Y | Y | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Time tracking or timer | ? | Y | ? | ? | Pomodoro | ? | Business plan | ? | Toggl | ? | Focus timer |
| Habits or routines | ? | ? | ? | ? | ? | Recurring tasks | ? | Y | ? | ? | Y |

**What each does best, on the checked evidence**
- **Fantastical:** input and breadth, [automatic set switching, mirroring, urgent alarms](https://flexibits.com/pricing?product=fantastical).
- **BusyCal:** [menu bar availability heat map, traffic alerts, tasks that carry forward, LAN sync](https://busymac.com/busycal/).
- **Vimcal:** [drag-and-copy availability, duplicate auto-merge, holds with offered-slot history](https://www.vimcal.com/pricing).
- **Reclaim:** [habits placed within windows, Smart Meetings](https://reclaim.ai/pricing).
- **Notion Calendar:** [dragging database dates from the calendar](https://www.notion.com/product/calendar).
- **Structured:** [visual timeline, inbox, automatic replanning of missed tasks, accessibility options](https://apps.apple.com/us/app/structured-daily-planner/id1499198946).
- **Morgen:** [task integrations across Notion, Todoist, Linear, ClickUp and Obsidian, plus Linux](https://www.morgen.so/pricing).
- **Etar:** [staggered overlap layout](https://github.com/Etar-Group/Etar-Calendar/releases), a small density idea.

## What power users praise and complain about

Subscription cost is the best-supported complaint. Recurrence, sync and time zone problems recur, but evidence
per app is weak.

- **Cost:** Fantastical is called "rent-seeking" ([Tsai comments](https://mjtsai.com/blog/2026/05/19/fantastical-at-15/)) [anecdotal].
  Some [moved to BusyCal to save money](https://setapp.com/es/apps/busycal/customer-reviews) [snippet, anecdotal].
- **Sync:** Fantastical [disappearing appointments and sync errors](https://kimola.com/reports/unlock-insights-fantastical-calendar-customer-feedback-report-app-store-us-148358) in an App Store analysis [snippet].
  Users often cannot tell whether the app or the provider is at fault, and Flexibits [points at the underlying service](https://flexibits.com/support/kb/100) [snippet].
- **Recurrence duplicates:** Google documents [imported recurring events syncing only the first occurrence, and duplicates from changes across daylight saving time](https://support.google.com/a/users/answer/139635?hl=en) [snippet].
- **Android:** Fossify [had stale data through a DAVx5 fork](https://github.com/FossifyOrg/Calendar/issues/1291) [snippet], and a later release [fixed CalDAV sync on Android 11+](https://github.com/FossifyOrg/Calendar/releases). Etar reminders [unreliable for one user](https://discuss.grapheneos.org/d/21243-foss-calendar-app-supporting-caldav-and-webcal/14) [anecdotal].
- **Cross-app themes** from [one May 2026 roundup](https://unstar.app/blog/fantastical-google-apple-outlook-notion-calendar-apps-ranked-2026): time zone errors, sync lag, paywalls, re-authentication, unreliable notifications. It has no sample size and contains factual errors, so treat these as hypotheses.

## What no app appears to fill

This is inference, not proven absence. Planned-versus-actual tracking already exists
([Toggl](https://toggl.com/features/online-calendar-planner), [Clockify](https://clockify.me/features/calendar) [vendor, snippets]),
and Sunsama [counts calendar events toward actual time](https://roadmap.sunsama.com/improvements/p/track-actual-time-based-on-calendar-integration) [snippet],
so that alone will not differentiate. The opening is the whole loop from plan to routine to tracked actual to learned estimate,
on top of first-class calendar quality. Morgen comes closest on breadth plus planning. The candidates are in
[`calendar-goals.md`](./calendar-goals.md#differentiators-what-dedicated-apps-cannot-do).

## Hands-on benchmark list

Published pages cannot settle these, so test them by hand on the reference apps. Start with Morgen, Sunsama, Toggl and Akiflow for the
differentiators, and Fantastical, BusyCal and Notion Calendar for the baseline.

- This, following and all recurrence editing.
- Floating time and travel behaviour.
- Multi-zone gutter design.
- Attendee and RSVP depth.
- Lock screen support.
- Multi-person overlays.
- Undo and search quality.
- Drag interactions and view density.
- Ad status of Google Calendar, Readdle and One Calendar's free tiers.
- Every "?" cell above.
