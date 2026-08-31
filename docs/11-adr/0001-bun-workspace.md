# 0001 — Bun workspace for app code

**Status:** accepted

## Context

We have multiple JS/TS packages: web client, desktop client UI, codegen scripts, integration tests. We need a workspace tool that handles install, link, and per-package scripts.

## Decision

Use **Bun** as the runtime and workspace manager for all JS/TS code in the repo.

## Alternatives considered

| Option | Pros | Cons |
|---|---|---|
| pnpm | Mature, widely adopted, fast | Yet another package manager to install; node-only |
| Turborepo + pnpm | Strong build orchestration | Heavier, multiple tools |
| npm/yarn workspaces | Stable | Slower install, weaker tooling |
| **Bun** | Fast install, fast test runner, Bun-native runtime; single tool | Younger; some compatibility quirks |

## Consequences

- Faster `install` and per-package scripts.
- The Sunrise core (Rust) is *not* in the Bun workspace; Cargo workspace manages that side.
- We commit to maintaining compatibility as Bun evolves; if a regression bites, we can fall back to pnpm with minimal disruption.
- CI uses Bun directly; no Node fallback unless a tool is incompatible.
