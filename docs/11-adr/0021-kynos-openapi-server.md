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
* `axum` leaves the workspace once ADR-0023 lands and the WebSocket goes with it.
  Until then the relay runs kynos for REST and bare hyper for `/sync`; this is a
  migration state, not a fallback, and it ends in the same epic.
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
