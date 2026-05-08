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

The server resolves the config file in this order: `--config <path>` flag → `$SUNRISE_CONFIG` env → `./sunrise.toml` → `/etc/sunrise/sunrise.toml`. **First found wins.** A missing config is a fatal error with exit code 78 (`CONFIG`).


```toml
[server]
listen = "0.0.0.0:443"
public_url = "https://sunrise.example.com"

[tls]
mode = "acme"            # or "static"
acme_email = "ops@example.com"
# ACME certs renew at expiry - 30 days as a background task (no restart).
# On renewal failure: retry hourly with exponential backoff up to 24h, then daily.
# Warnings are logged at 14, 7, and 3 days remaining.

[storage]
mode = "single-binary"   # or "scaled"
data_dir = "/var/lib/sunrise"
sqlite_pool_size = 5      # WAL mode; writes serialize at the SQLite layer.
                          # The server retries `database is locked` up to 3 times
                          # with 50 ms backoff before returning 503 SERVER_OVERLOADED.

# OR for scaled:
# postgres_url = "postgres://..."
# s3_endpoint = "https://s3.example.com"
# s3_bucket = "sunrise-ops"

[auth]
oidc_issuer       = "https://auth.example.com"   # required; any OIDC-conformant issuer
oidc_client_id    = "sunrise"
oidc_client_secret = "..."
allow_signup      = false                        # if false, only existing IdP users with prior accounts can use the server
admin_emails      = ["ops@example.com"]

[push]
apns = { key_id = "...", team_id = "...", key_path = "..." }   # optional
fcm  = { service_account_path = "..." }                         # optional
web_push = { vapid_public_key = "...", vapid_private_key = "..." }
# Relative paths in [push.apns] and [push.fcm] resolve against the config file's
# directory (NOT the CWD). Absolute paths are used as-is.

[observability]
audit_retention_days = 30   # default; cron at 02:00 UTC deletes expired records.
                            # Server logs use account_h everywhere; no email-tagged
                            # buffer exists — there is no auth_log_retention_hours
                            # setting in v1.

[quotas]
max_account_storage_mb = 51200
max_ops_per_minute = 600
max_devices = 50
max_blob_size_mb = 100
```

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
