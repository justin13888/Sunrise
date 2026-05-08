---
status: draft
---

# Personas

Sunrise targets one core persona with two adjacent variants. We refuse to dilute the core for the variants.

## Core: The Multi-Stream Operator

**Profile.** 28–45 years old. Knowledge worker. Has at least three of: a primary job with multiple projects, a side income or consulting stream, a serious hobby/creative practice, family obligations (kids or aging parents), an active travel cadence, ongoing health/fitness routines. They are *competent* with tooling — they have tried Notion, Things, Todoist, Linear, Obsidian — and they have one specific complaint each.

**Pains.**
- Their task system collapses under the weight of cross-stream context. Work tasks bleed into personal lists.
- They distrust cloud-only tools after seeing one shut down or pivot.
- They've felt the cost of a sync conflict in another tool and now manually reconcile.
- They care about privacy enough to read a privacy policy, but not enough to run their own server unless setup is a 5-minute task.

**Wins.**
- Open Sunrise, see "today across all streams," not "all 412 tasks across all projects."
- Capture from anywhere — TUI on a remote box, lock screen on iOS — and trust it shows up everywhere.
- Hand off work mid-flight (laptop → phone) without thinking about sync.

## Variant A: The Solo Founder / Operator

Same profile, but everything is more concentrated. One company, many functions (sales, product, finance, ops). Streams are *functions* not *jobs*. They share one or two views with a co-founder or assistant. **Sharing must work without the recipient creating an account is a stretch goal — see [`03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md).**

## Variant B: The Researcher / Grad Student

Long-running projects (multi-year), heavy reading queue, recurring weekly review, citations and notes attached to tasks. Mostly solo. Wants TUI on a research compute box. Cares deeply about export and longevity.

## Explicitly *not* targeted

- **Teams of >3.** Sunrise is not a Linear/Asana replacement. Cross-user sharing exists but is not a collaboration product.
- **Casual single-list users.** Apple Reminders is fine for them. We are not competing on "lower the floor."
- **Enterprise / SOC-2 buyers.** Self-host is the answer. We are not building org-admin features in the foreseeable future.

## Implications for design

- The *first* screen is "Today, across streams" — not a project picker.
- Quick capture exists on every surface, including TUI and iOS lock screen.
- Recurrence and routines are first-class, not bolted on.
- Sharing is an integrity-preserving extension, not the core loop.
