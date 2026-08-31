# ADR 0010 — Explicit protocol versioning spec

**Status:** accepted

## Context

ADR-0004 established the crypto suite and noted that op envelopes carry `aead_alg`, `sig_alg`, and `epoch` fields "so future rotation is a clean version transition." Section 05-sync/wire-protocol.md mentions a `PROTOCOL_VERSION_MISMATCH` error code. Section 02-domain/schema-versioning.md describes forward-compat rules for unknown CRDT fields.

These were three local treatments of versioning. The audit revealed that no document tied them together: there was no canonical list of version constants, no spec for how a client and server negotiate at session start, no capability-bit registry for optional features, and no deprecation policy. A v1 implementer attempting to be safe across future versions would have to invent the negotiation protocol.

## Decision

Add `docs/10-cross-cutting/protocol-versioning.md` as the authoritative versioning spec. It:

1. Names the four versioned surfaces (wire, doc-schema, crypto-suite, storage).
2. Pins v1 constants for each.
3. Specifies the `Hello` / `HelloAck` negotiation handshake including CDDL.
4. Defines a 64-bit capability bitfield with a v1 registry.
5. Sets evolution rules per surface (when a bump is required, deprecation windows, telemetry gates for closing a window).
6. Specifies magic prefixes for every persisted structure.
7. Lists version-mismatch error codes.
8. Mandates byte-exact test fixtures for negotiation paths.

The spec lives in `10-cross-cutting/` because all four surfaces depend on it; it is not bound to any single section.

## Alternatives considered

1. **Keep versioning rules per-surface.** Rejected: cross-surface bumps (e.g., new crypto suite that requires a new wire kind) had no clear handling.
2. **Single monolithic version number.** Rejected: forces lockstep evolution; a wire-protocol fix would force a doc-schema bump.
3. **Implicit version negotiation via feature detection.** Rejected: feature detection works for capabilities but not for incompatible primitives; a server that no longer speaks v1 crypto must say so explicitly.

## Consequences

**Positive:**
- Server and client share one source of truth for versioning rules.
- Deprecation windows are tied to telemetry gates (cannot drop a version while > 0.5% of devices still use it).
- New optional features go through capability bits without a version bump.
- Test fixtures pin every negotiation outcome to a byte sequence.

**Negative:**
- More upfront ceremony for each protocol change (must update version constants, update fixtures, update telemetry dashboards).
- Server must support the previous wire-proto for ≥ 90 days after a bump.
