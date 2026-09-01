# 0021 — `kynos` and an authoritative OpenAPI 3.2 document replace hand-written axum routing

**Status:** accepted

**Supersedes** the implicit "axum plus hand-written handlers" arrangement in
[`docs/06-server/api.md`](../06-server/api.md).
**Forces** [ADR-0022](./0022-device-signature-canonical-json.md) (request signing)
and [ADR-0023](./0023-sse-sync-transport.md) (sync transport).

## Context

The relay's HTTP surface had no machine-readable contract of any kind. Fourteen
routes were registered by hand in `Router::new()` calls spread across five
modules, and the only schema file in the repository — `schemas/log-record.v1.json`
— describes log records, not the API. The CDDL blocks in `06-server/api.md` are
prose: nothing reads them, nothing validates against them, and the audit that
preceded this ADR found nine separate places where they had drifted from the
handlers, including a documented `POST /api/v1/devices/<dev_id>/push_token` that
is really `POST /api/v1/devices/push-tokens`, and a documented Postgres hard
delete that is really a SQLite soft delete.

Two consequences followed, and both were live:

* **No client existed.** Nothing in the workspace called the REST API — not the
  CLI, not the macOS app, not `sunrise-core-bindings`. `sunrise-onboarding`
  declares `AccountCreateRequest` and `AccountInfo` and makes no HTTP call with
  them. The account-and-device bootstrap flow the crypto design depends on was
  specified, served, and never invoked, and nothing detected that.
* **The handlers were untyped at the body.** Every authenticated handler took
  `body: Bytes` and called `serde_json::from_slice` itself, because those exact
  bytes are what `X-Sunrise-Device-Sig` covers and deserialising first would
  leave the signature checking a re-serialisation. The reasoning was sound; the
  result was that the request shape existed only inside each handler.

Adding a document generator to the existing arrangement would have documented
the drift rather than removed it. A generator that reads `Bytes` learns nothing.

## Decision

**`kynos` is the server framework, `spargen` generates the client, and the
emitted OpenAPI 3.2 document is authoritative.** No fallback path is retained:
there is no second routing layer, no hand-written client, and no hand-maintained
schema alongside the generated one.

* **`kynos`** (`0.1.0`, MIT, MSRV 1.85) is a tokio/hyper REST framework whose
  premise is that a handler which cannot be described does not compile. Typed
  extraction covers path, query, headers, cookies, JSON, forms and multipart;
  security schemes are declared with `#[derive(SecurityScheme)]`; errors are
  RFC 9457 problem details.
* **`spargen`** (`0.4.0`, MIT OR Apache-2.0, MSRV 1.88) generates the Rust
  client — typed models, one method per operation, typed errors — over a
  swappable `HttpBackend`. The generated client is what finally gives the
  bootstrap flow a caller, and it reaches Swift through the existing UniFFI seam
  rather than needing a second generator.

Both are first-party (`github.com/getkono`), so a gap in either is a fix we can
make rather than a constraint we work around.

`kynos` shapes two decisions that are recorded separately because they change
the wire, not just the code:

1. **It has no `Request`, `Body` or `HeaderMap` extractor.** Its README names
   these as "exactly the holes through which aide and utoipa emit documents with
   silent gaps". The escape hatch, `Unchecked<T>`, marks the operation's
   generated documentation non-authoritative — which on this codebase would mark
   precisely the security-critical routes, since those are the signed ones. That
   forced [ADR-0022](./0022-device-signature-canonical-json.md).
2. **It excludes bidirectional streaming permanently** — "OpenAPI describes HTTP
   request/response semantics. A stream that stops being either belongs to
   AsyncAPI." That forced [ADR-0023](./0023-sse-sync-transport.md).

## Alternatives considered

| Option | Why not |
|---|---|
| **`utoipa` / `aide` on axum** | Both derive the document from types the handler declares. Every signed route declares `Bytes`, so the document would have described the API's authenticated half as an opaque blob — the exact silent gap that motivated adopting a framework at all. Fixing that still requires ADR-0022, at which point axum's advantage is only that it is already there. |
| **Keep axum, hand-write an OpenAPI document** | A second artefact to drift from. The audit found nine drifted CDDL blocks maintained exactly this way; there is no reason to expect a tenth to fare better. |
| **`Unchecked<T>` on signed routes** | Retains byte-exact signing with no protocol change, and marks the account, device and blob routes non-authoritative in the generated document. That is the subset a client most needs described, so it removes most of the value while paying the migration cost. |
| **OpenAPI 3.0/3.1** | 3.2's sequential media types and `itemSchema` are what let the sync stream be described at all (ADR-0023). Choosing an older version would leave sync outside the document permanently, and `spargen` rejects 3.0.x regardless. |
| **gRPC / tonic** | Strongly typed with codegen, but not OpenAPI-describable, and it introduces protobuf beside CBOR. The canonical-CBOR encoding is load-bearing for envelope signing, so a second encoding is a second thing to keep canonical. |

## Consequences

* The OpenAPI 3.2 document is generated from the handlers and is the contract.
  A route that cannot be described does not compile, so the drift class the
  audit found cannot recur by the mechanism that produced it.
* `spargen` output replaces no hand-written code, because none existed. It gives
  the account/device bootstrap its first caller.
* `axum` leaves the workspace **in this ADR's own migration**, not with
  ADR-0023. See the addendum below.
* `tower-http`'s `TraceLayer`, `CorsLayer` and `RequestBodyLimitLayer` need
  kynos equivalents. The query-string scrubbing in `logging/mod.rs` is not
  optional decoration — it exists because a bearer token reached the log once
  already — and its regression test must survive the port unchanged.
* MSRV is unaffected: the workspace pins 1.88.0 and `spargen` needs 1.88.
* `kynos` is `0.1.0`, published the day before this ADR. Its core — document
  model, schema, routing, handlers, dependency injection, server — is declared
  frozen; CORS, compression, cookies and rate limiting are "settling". The
  migration therefore depends on two behaviours that must be confirmed before
  the port begins rather than discovered during it: that kynos composes with a
  layer able to observe a request before dispatch, and that its SSE support can
  serve a long-lived stream fed by the relay's broadcast hub with mid-stream
  auth expiry.

## Addendum — `upgrade_unchecked`, and no second stack

Written against the crate's README and feature list. Both open questions above
were then checked against the API before any route was ported, and one of them
changed this decision.

**The two questions, answered.**

* *Does kynos compose with a layer that can observe a request before dispatch?*
  Yes, as an `Interceptor`, which runs after routing and declares what it reads
  and adds. Mounting kynos *outward* into an existing stack is free; tower layers
  placed *inward* need a declaration or a waiver.
* *Can its SSE serve a broadcast-fed stream with mid-stream auth expiry?* Yes.
  `Sse<S>` wraps any `futures_core::Stream<Item = Result<Event<T>, E>>`, pulls
  one event at a time with no buffering ahead, drops `S` when the client goes
  away, and keeps the connection alive on a configurable interval.
  `extract::sse::LastEventId` is the receive half, which is what
  [ADR-0023](./0023-sse-sync-transport.md) maps the relay's durable cursor onto.

**What changed.** This ADR assumed `/sync` would need bare hyper beside kynos
for the length of the migration, because kynos cannot describe a WebSocket.
`Router::upgrade_unchecked` exists for exactly that case — "protocol upgrades
away from HTTP, primarily WebSockets", served only on `GET` and recorded as
`OpaqueReason::ProtocolUpgrade`. So `/sync` is one operation carrying one named,
auditable waiver, rather than a second HTTP stack running beside the first.

That is materially better than both alternatives considered:

* it is **not** `into_tower_unchecked`, which flags *every* operation as
  `OpaqueReason::UntypedLayer` — the reason this ADR rejected wrapping in the
  first place, applied to the whole surface instead of one route;
* it removes the two-stack migration state entirely, so `axum` and
  `tokio-tungstenite` leave with this change rather than with ADR-0023.

`unchecked_reasons()` returns the waivers taken, deduplicated, so CI can assert
that `ProtocolUpgrade` on `/sync` is the *only* one — a much stronger gate than
"no waivers", which a project with a legitimate upgrade route cannot hold.

**One correction to the decision text.** `openapi()` emits the lowest version
that expresses the surface — `3.1.2` for a document with no 3.2-only construct
in it — so "the emitted OpenAPI 3.2 document" is only true if 3.2 is *asked*
for. `openapi_as(SpecVersion::V3_2)` targets rather than downgrades: it fails
and names what blocks it instead of quietly emitting an older version. That is
what the build calls, and what `the_router_describes_itself` asserts.


## Second addendum — the first one was written against a README

The addendum above was, in its own words, "written against the crate's README
and feature list". Both of its load-bearing claims turned out to be wrong when
the port reached the code, and the correction is recorded here rather than
quietly applied, because the reasoning it replaced is the kind that reads
convincingly and is not checkable from a README.

**`upgrade_unchecked` was never available, and would not have worked.**

1. It lives behind kynos's `unchecked` feature (`unchecked = ["dep:tower",
   "dep:tower-service"]`), which this workspace does not enable and which
   `server` does not pull in. `upgrade_unchecked`, `unchecked_reasons`,
   `has_unchecked` and `into_tower_unchecked` do not exist in this build — so
   the CI gate the addendum specified could not have been written, let alone
   held.
2. Even with the feature on, `kynos::server` calls hyper's `serve_connection`
   rather than `serve_connection_with_upgrades`, so a WebSocket handshake cannot
   complete. kynos's own `examples/unchecked.rs` answers `501` and says as much:
   "a real upgrade hands the connection to a socket driver."

A working `/sync` WebSocket would therefore have needed the feature enabled
*and* a hand-written accept loop over `Service::call` with direct `hyper` and
`tokio-tungstenite` dependencies — which is a second HTTP stack again, the thing
the addendum believed it had removed.

**What actually happened.** [ADR-0023](./0023-sse-sync-transport.md) landed in
the same change, so `/sync` is describable and no waiver is taken anywhere. The
gate is therefore the strict one this ADR called "unholdable for a project with
a legitimate upgrade route": `.github/scripts/kynos-waiver-gate.py` asserts the
`unchecked` feature is off, which is stronger than inspecting
`unchecked_reasons()` because a hatch that does not compile cannot be reached by
oversight either. `axum` and `tokio-tungstenite` did leave with this change, as
the addendum predicted — for a different reason than it gave.

**A second correction, to this ADR's own Decision text.** It says kynos "seals"
nothing of the sort, but the *port* recorded a claim that it did: that
`Describe` is sealed and `OperationCx` private, so a custom extractor cannot
describe itself. That is false. `Describe` is a plain public trait carrying only
a `diagnostic::on_unimplemented`, and `OperationCx` exposes `new`, `finish`,
`add_parameter`, `set_request_body`, `add_security` and the rest. kynos's own
`LastEventId` is a hand-written extractor and `examples/parameters.rs`
advertises one. The device binding is `Signed<T>` — an extractor that parses and
verifies in one step — for that reason.

**A gap this ADR did not anticipate.** `spargen` rejects the way kynos describes
a raw binary body. kynos emits the empty Schema Object, citing the shape
OpenAPI 3.1 describes "by omitting things"; spargen requires a string-like or
binary schema and refuses with `E009`. Both are first-party
(`github.com/getkono`), so this ADR's own framing applies — "a gap in either is
a fix we can make rather than a constraint we work around" — but until the two
agree, the chunk `PUT` and the blob `GET` are omitted from the generated client
with the reason recorded at the omission. No client uploads a chunk today, so
nothing is currently lost.
