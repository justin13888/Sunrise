---
status: accepted
---

# Automation Rules

Limited "if-this-then-that" rules. Power users configure them; they run on-device.

## Scope

- **Triggers:** task created in Stream X / context Y added / task overdue by N hours / routine occurrence missed / weekly review run.
- **Conditions:** stream is/isn't, has/doesn't-have context, priority above/below, due-within range.
- **Actions:** add context, set priority, move to Stream, schedule for, defer N hours, send local notification, run a saved search and surface count, run an external webhook (with safety controls).

## Examples

- *When* a task is created in Stream `Work A` *and* contains `@waiting-on:carlos`, *then* schedule a follow-up reminder in 3 days.
- *When* an Inbox task has been there > 7 days, *then* surface in weekly review even if not normally surfaced.
- *When* a routine occurrence misses the day, *then* move it to "this week" and add `@catchup`.

## Definition

```cddl
Rule = {
    id:       tstr,
    name:     text<128>,
    enabled:  bool,
    when:     Trigger,
    if:       [* Condition],
    then:     [+ Action],
    last_run: tdate,
}
```

Rules are CRDT-synced.

## Execution

- Triggered by domain events emitted by the core (the same `changes()` stream the UI subscribes to).
- Run **once per device** by the device that holds the "primary scheduler" role (otherwise we'd run actions twice per multi-device user).
- Idempotency keys on any state-changing actions to dedup if multiple devices momentarily race.

## Webhooks (advanced)

Out-of-band integrations:

- HTTP POST to a user-configured URL with a small JSON payload (entity ID, kind, event).
- Body is signed with a per-rule HMAC the user shares with the destination.
- **Content is opt-in.** By default, the payload includes only IDs and kinds, not titles/bodies. The user can opt in to including titles for specific rules.

## Safety

- A single trigger fires **at most one** action chain by default. Setting `allow_batch = true` on a rule raises the cap to 100 affected entities per fire; rules requesting more are rejected at save time.
- Rule preview: the user sees a dry-run of how many entities a rule *would* affect before enabling it.
- Disabled rules are paused, not deleted, so the user can experiment.

### Loop prevention

A rule's actions can produce ops that are themselves triggers (e.g. `move-to-Stream` triggers a "task created in Stream" rule). To avoid runaway feedback:

- Each action carries a `triggered_by_op_id` field; the automation engine refuses to fire any rule whose chain depth on a single originating op exceeds 4.
- A rule that has fired N times in the last 60 seconds for the same target entity is auto-disabled with a UI warning (default N = 5; configurable per rule, range 1–20).
- Routines do NOT trigger automation rules during their initial materialization burst on a fresh device (cold start emits hundreds of `create` ops; treating them all as triggers would explode).

## What we don't do

- Turing-complete scripting. The action set is a fixed enum.
- Cloud-side execution. Rules run on the user's device.
- Cron-triggered rules with arbitrary times (we use the existing event stream as the trigger source).

## Future

- Saved-search-driven triggers.
- Conditional chains (if action A succeeds, then action B).
- Tracked but not in v1.
