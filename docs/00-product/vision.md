---
status: accepted
---

# Product Vision

## What Sunrise is

Sunrise is a **personal operating system for people who run many parallel streams of work**. The user is not someone managing one job and a grocery list — they are juggling: one or two jobs, a long-running side project, household chores, relationship obligations, an upcoming international trip, recurring health routines, and three creative projects in different states of dormancy.

Existing tools (Todoist, Things, OmniFocus, Notion, Apple Reminders) optimize for *adding tasks*. Sunrise optimizes for **keeping multiple long-lived streams in healthy motion** without the user mentally context-switching between tools or worrying about whether their data is safe.

## Design tenets

These are **non-negotiable**. Any feature decision that conflicts with one of these tenets is wrong.

1. **Local-first.** The local copy is authoritative. The network is a sync transport, not a dependency. Every action works offline; every action commits locally first.
2. **End-to-end encrypted.** The server cannot read user data. Keys live on devices. A server compromise leaks ciphertext, sync metadata, and timing — never plaintext content.
3. **Multi-client by construction.** The same user runs Sunrise on more than one surface — today a Mac app and a command line, later a phone. State converges automatically, and **no client is "the real one"**: the core is, and every client is a view of it. This tenet is about the *architecture*, not a promise about which platforms ship; the shipping list lives in [`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md).
4. **Multi-stream first.** The model is built around *streams of work* (Work A, Work B, Family, Travel, Health, …) as first-class. Filtering and review flows treat streams as the primary axis.
5. **Latency is a feature.** Sub-50ms perceived latency on every interaction — capture, mark-done, navigate. No spinners on the local data path.
6. **Open and self-hostable.** The sync server is open source. Users can self-host. The wire protocol is documented and stable.
7. **Disciplined scope.** The product refuses features that turn it into a CRM, wiki, or chat tool. See [`non-goals.md`](./non-goals.md).

## What success looks like

- A user can do their morning planning entirely offline on a flight.
- A user can lose their phone in a river, set up Sunrise on a new phone with their recovery code, and have their data within minutes — with the server having seen nothing but ciphertext throughout.
- A user can `ssh` into a box, run `sunrise capture '…'`, and see the task on their other devices before they finish typing the disconnect. (Capture, triage, review and export are scriptable; there is no interactive terminal client — see [ADR-0019](../11-adr/0019-swiftui-macos-client.md).)
- A user can export everything (decrypted) at any time, in a format that makes sense outside Sunrise.

## What success does *not* look like

- A user has to learn a query language to filter their tasks.
- A user feels obligated to maintain Sunrise (groom backlog, archive, tag) as a chore in itself.
- A casual user has to learn the system before they can capture and triage on day one.
