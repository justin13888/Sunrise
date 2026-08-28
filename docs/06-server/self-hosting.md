---
status: accepted
---

# Self-Hosting

A first-class deployment topology, not a charity afterthought.

## Distribution

- Single binary: `sunrise-server`.
- OCI image: `ghcr.io/<org>/sunrise-server:<version>`.
- One config file: `sunrise.toml`.

## Minimum viable host

- 1 vCPU, 1 GB RAM, 20 GB disk for ≤10-user instance.
- TLS via either the binary's built-in ACME (LetsEncrypt) or operator-provided cert.
- Outbound network for ACME and (optionally) push provider connections.

## Config (`sunrise.toml`)

The server resolves the config file in this order: `--config <path>` (also
`-c <path>` and `--config=<path>`) → `$SUNRISE_CONFIG` → `./sunrise.toml` →
`/etc/sunrise/sunrise.toml`. **First found wins.**

A path named by the flag or the env var that cannot be read is a fatal error
with exit code 78 (`EX_CONFIG`): naming a file is how an operator says the
defaults are wrong, so falling back to them would run a server nobody asked
for. Finding *no* config at all is **not** an error — the server then runs on
its defaults, which bind loopback in single-tenant mode. A config that parses
but describes an unsafe server (see "Refusals" below) also exits 78.

Every key is optional and overlays the defaults, so a file that sets one key
changes one thing.

```toml
[server]
listen           = "0.0.0.0:443"          # default "127.0.0.1:8443"
allowed_origins  = ["https://app.example.com"]   # browser origins; must be scheme-qualified
max_body_bytes   = 2097152                # default 2 MiB

[auth]
oidc_issuer        = "https://auth.example.com"  # must be https
oidc_client_id     = "sunrise"                   # tokens must carry it in `aud`
allow_signup       = false                # default true
require_device_sig = true                 # default false; requires an issuer
token_leeway_secs  = 60                   # clock-skew allowance on exp/nbf
jwks_ttl_secs      = 300                  # cache TTL for a JWKS with no cache headers

[storage]
data_dir = "/var/lib/sunrise"             # expands to <dir>/sunrise.db and <dir>/blobs
                                          # unset = ephemeral in-memory (tests only)
```

Setting **both** `oidc_issuer` and `oidc_client_id` is what installs the JWKS
verifier. With either missing the server stays single-tenant, where every
caller maps to the same account.

### Refusals

The server exits 78 rather than starting, when:

| Condition | Why |
|---|---|
| Single-tenant and `listen` is not loopback | Publishes one shared account namespace to the network |
| `oidc_issuer` set without `oidc_client_id` | Tokens could not be audience-checked |
| `oidc_issuer` is not `https://` | Bearer tokens over plaintext |
| `require_device_sig` without an issuer | Device signatures are meaningless under the self-host verifier |
| An origin is `*` or not scheme-qualified | Ambiguous CORS |
| `max_body_bytes = 0` | Rejects every request |

Unknown keys and unknown tables are **rejected**, not ignored. Writing a
`[tls]` block and having it silently dropped would serve plaintext while the
operator believed otherwise, so the parser names the offending key instead.

### Not yet wired

These appear in earlier drafts of this document and are **not implemented**;
the parser will reject them rather than accept them silently:
`[tls]` (terminate TLS at a reverse proxy for now), `[push]`, `[quotas]`,
`[observability]`, `[storage] mode` / `sqlite_pool_size` / `postgres_url` /
`s3_*`, `[server] public_url`, `[auth] oidc_client_secret` / `admin_emails`,
and the `sunrise-server doctor` subcommand.

## Operator surfaces

| Surface | Purpose |
|---|---|
| `/api/v1/admin/*` (loopback only by default) | Health, stats, manual blob GC, user actions |
| Prometheus `/metrics` | Aggregate metrics (no per-account labels with PII) |
| Logs (stdout) | Structured JSON; never contains content; never contains push tokens |

## Backup

- Single-binary: stop, `tar czf` the data dir, start. Or use a snapshot-aware filesystem (ZFS, Btrfs).
- Scaled: standard Postgres + S3 backup tooling.

## Upgrade

- Stop, replace binary, start. Migrations run on startup.
- Zero-downtime upgrade for scaled deployments via standard rolling restart.

## Testing the install

The binary includes a `sunrise-server doctor` subcommand:

- Verifies TLS works.
- Verifies storage is writable: writes a 10 MiB test file under `[storage] data_dir`, fsyncs, reads back, asserts byte-identical, and deletes. Reports the free-space ratio: warns at < 10%, errors at < 1%.
- Verifies Postgres `fsync = on` (production requirement).
- Verifies push providers are configured (if enabled).
- Reports protocol versions supported.
- Round-trips a synthetic op end-to-end against a built-in test client.
- `sunrise doctor --check-logs` verifies that audit/log retention is being enforced as configured.

## What self-hosters give up vs managed cloud

| Feature | Managed | Self-host |
|---|---|---|
| Push reliability | High (Sunrise-operated APNs/FCM) | Operator-managed; optional |
| Cross-server sharing | n/a (v1 only same-server) | n/a |
| Capacity scaling | Auto | Operator-driven |
| Backups | Sunrise-managed | Operator-managed |

Functional features (E2EE, sync, multi-client, sharing within the same server) are **identical**.

## Migrations between topologies

A managed-cloud user can move to self-host:

1. Spin up self-host.
2. Add the new server URL to a trusted device's settings.
3. The device "re-pairs" against the new server, uploading its full op log.
4. Other devices update their server URL setting; they re-sync.
5. Cancel managed account; managed server purges data.

The user's identity and data are unchanged (clients are authoritative).
