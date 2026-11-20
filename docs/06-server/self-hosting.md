---
status: accepted
---

# Self-Hosting

A first-class deployment topology, not a charity afterthought.

## Distribution

- Single binary: `sunrise-server`.
- OCI image: `ghcr.io/<org>/sunrise-server:<version>`, where `<version>` is the core train's, released by a `core-v<version>` tag ([ADR-0064](../11-adr/0064-two-version-trains.md)).
- One config file: `sunrise.toml`.

## Minimum viable host

- 1 vCPU, 1 GB RAM, 20 GB disk for ≤10-user instance.
- **TLS terminates at a reverse proxy.** The binary has no ACME client and no
  certificate loading of any kind; a `[tls]` block is rejected by the config
  parser rather than silently ignored, precisely so nobody believes they are
  serving HTTPS when they are not. "Reverse proxy" below walks through the two
  tested configurations that ship in `deploy/`.
- Outbound network for the OIDC issuer's discovery and JWKS documents (HTTPS
  only — `HttpsFetch` refuses a plaintext URL before dialling).

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
shutdown_grace_secs = 25                  # default 25; how long SIGTERM waits for
                                          # in-flight requests; at least 1 (0 is refused)
trusted_proxies  = ["127.0.0.1"]          # default []; reverse proxies whose X-Forwarded-For
                                          # is believed; addresses or CIDRs

[auth]
oidc_issuer        = "https://auth.example.com"  # must be https
oidc_client_id     = "sunrise"                   # tokens must carry it in `aud`
allow_signup       = false                # default true
require_device_sig = true                 # default: on with an issuer, off without one;
                                          # true without an issuer is refused, and false
                                          # with one logs srv.start.device_sig_optional
token_leeway_secs  = 60                   # clock-skew allowance on exp/nbf
jwks_ttl_secs      = 300                  # cache TTL for a JWKS with no cache headers

[storage]
data_dir = "/var/lib/sunrise"             # expands to <dir>/sunrise.db and <dir>/blobs
                                          # unset = ephemeral in-memory (tests only)
busy_timeout_ms = 5000                    # default 5000; how long SQLite waits on another
                                          # process's lock before failing; 0 fails at once
gc_grace_days = 30                        # default 30; a tombstoned blob is kept at least this
                                          # long, and until every active device acknowledged it
account_delete_grace_days = 30            # default 30; from a confirmed account deletion to
                                          # its erasure
pending_upload_ttl_hours = 24             # default 24; an upload untouched this long is swept
maintenance_interval_secs = 3600          # default 3600; how often the maintenance pass runs
encrypt  = true                           # default false; SQLCipher-encrypt sunrise.db at rest
key_file = "/etc/sunrise/db.key"          # required with encrypt; 64 hex digits, mode 0600,
                                          # outside data_dir (openssl rand -hex 32)
backend  = "sqlite"                       # default "sqlite"; "postgres" runs the relay as
                                          # several nodes (see "Several nodes over Postgres")
postgres_url_file = "/etc/sunrise/pg.url" # postgres only, and required there; one connection
                                          # URL, mode 0600
postgres_ca_file  = "/etc/sunrise/pg-ca.pem"  # postgres only, optional; set = every connection
                                          # is TLS, verified against these PEM roots alone

[limits]                                  # api.md §Rate limits; every key defaults
enabled              = true               # false turns every limit off (load tests only)
probe_per_min        = 120                # per client address: /health, /metrics
meta_per_min         = 60                 #   /meta
bootstrap_per_min    = 10                 #   POST /accounts, POST /devices
account_per_min      = 60                 #   account and device management
blob_per_min         = 600                #   /blobs/*
sync_per_min         = 600                #   /sync/*
failed_auth_per_5min = 20                 # 401s per address before its credentials are
                                          # refused unverified
ops_per_sec          = 50                 # per device; bursts to ten seconds' worth
blob_upload_bytes_per_min   = 67108864    # per device (64 MiB)
blob_download_bytes_per_min = 268435456   # per device (256 MiB)
open_uploads         = 16                 # per account, between init and finalize
sessions_per_5min    = 10                 # per device
streams              = 4                  # event streams per device at once

[push.apns]                               # optional; absent = no wake-up pushes
key_path    = "/etc/sunrise/AuthKey_ABC123DEFG.p8"   # the .p8 key Apple issued; mode 0600
key_id      = "ABC123DEFG"                # the key's 10-character Key ID (JWT `kid`)
team_id     = "DEF123GHIJ"                # 10-character Apple Developer Team ID (JWT `iss`)
topic       = "dev.sunrise.app"           # the app's bundle id, sent as `apns-topic`
environment = "production"                # "sandbox" for development builds' tokens

[observability]                           # optional; absent = no tracing at all
endpoint     = "http://127.0.0.1:4318/v1/traces"   # required; the collector's OTLP/HTTP
                                          # traces URL, used verbatim; http:// or https://
sample_ratio = 0.01                       # default 0.01; 0.0 to 1.0, and also the cap on
                                          # what a client's traceparent can ask for
service_name = "sunrise-server"           # default "sunrise-server"; `service.name`
deployment   = "production"               # optional; `deployment.environment.name`
```

`[observability]` exports OpenTelemetry traces of the relay's requests, sync
streams and push deliveries to a collector over OTLP/HTTP (protobuf bodies),
redacted by construction: no span carries an id, a token, a path or a body
([`observability.md`](./observability.md) §Tracing lists what each span holds).
Written at all, the table turns export on, and `endpoint` is the one key it
needs. `sample_ratio` defaults to the 1% production figure; a staging relay that
wants every trace sets `1.0`. A client's `traceparent` joins its trace to the
relay's and can lower that ratio, never raise it. Plain `http://` suits a
collector on the same host; send to one elsewhere over `https://`. The
exporter also honours the standard `OTEL_EXPORTER_OTLP_TRACES_HEADERS` for a
collector that wants an authorization header, and the `OTEL_BSP_*` batch
settings. Without the table no exporter is built and no span is recorded.

`[push.apns]` turns on content-less wake-ups for iOS devices that have no event
stream open (see [`push-notifications.md`](./push-notifications.md)). Written at
all, it is written whole: every key is required, `environment` has no default
because a development build's tokens only work against the sandbox gateway,
and `[push.fcm]` or any other provider table is rejected as unknown. The key
file must be readable by its owner alone — `chmod 600` (or `400`) and owned by
the user the relay runs as. Leave it out of backups of the data dir; it is
config, and a leaked copy signs pushes for the app. Without `[push]`, the
server logs `srv.push.disabled` once at startup and sends nothing.

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
| A `[limits]` value is `0` (the error names the key) | Refuses everything that limit covers; `enabled = false` is how limits are turned off |
| A `trusted_proxies` entry is not an address or CIDR network, or is a `/0` | Hostnames are not resolved; the socket reports an address. A `/0` would believe a forwarding header from any client |
| `busy_timeout_ms` above 2147483647 | SQLite holds the timeout in a 32-bit signed integer of milliseconds (about 24 days) |
| `pending_upload_ttl_hours = 0` or `maintenance_interval_secs = 0` | The first would sweep uploads still in flight; the second would run the maintenance pass in a busy loop. The two grace periods may be `0`. |
| `encrypt = true` without `key_file`, or `key_file` without `encrypt = true` | Either the database would not be encrypted while the operator believes it is, or there is no key to encrypt it with |
| `key_file` is missing, not 64 hex digits, readable by group or others, or inside `data_dir` | It opens the whole database; inside the data dir, every copy of the data dir would carry it |
| `sunrise.db` is encrypted and `encrypt` is off, or `key_file` holds a different key | The relay cannot read it; the log names which (see "Encryption at rest") |
| `sunrise.db` is at a schema version newer than this binary's | A newer release migrated it; writing to it could corrupt what that release relies on (see "Upgrade") |
| `backend = "postgres"` without `postgres_url_file`, without `data_dir`, or beside `encrypt = true`; a `postgres_*` key beside `backend = "sqlite"` | The relay would have no database to reach, blobs in a private temp dir on each node, or an encryption switch that encrypts nothing; a Postgres key on SQLite reads nothing |
| `postgres_url_file` is missing, readable by group or others, or not a Postgres URL; the URL asks for `sslmode=require` with no `postgres_ca_file`; the CA file holds no PEM certificate | It holds the database password; TLS with nothing to verify the server against would be TLS in name only |
| The Postgres database cannot be reached, or is at a schema version newer than this binary's | As for `sunrise.db`: nothing is written to a database a newer release migrated |
| `[push.apns] key_path` is readable by group or others, missing, or not a P-256 `.p8` key | It signs pushes for the whole app; a key that cannot sign would fail every push instead of the start |
| `[push.apns] key_id` or `team_id` is not 10 uppercase letters and digits, or `topic` is empty | APNs would refuse every provider token or push |
| `[observability] endpoint` is not an `http://` or `https://` URL with a host, `sample_ratio` is outside `0.0`–`1.0`, or `service_name` is empty | The exporter could not send, or would sample a fraction that is not one |

Unknown keys and unknown tables are **rejected**, not ignored. Writing a
`[tls]` block and having it silently dropped would serve plaintext while the
operator believed otherwise, so the parser names the offending key instead.

## Reverse proxy

The relay serves plain HTTP and nothing else, so a public relay always sits
behind a reverse proxy that terminates TLS. Two tested configurations ship in
[`deploy/`](../../deploy/): [`deploy/caddy/Caddyfile`](../../deploy/caddy/Caddyfile)
and [`deploy/nginx/sunrise.conf.template`](../../deploy/nginx/sunrise.conf.template).
The `Deploy test` CI job boots each one unchanged in its official image, in
front of a real relay, and checks every property listed below from client
containers with addresses of their own (`deploy/test/run.py`). Start from one of
them rather than from a blank file.

### 1. Run the relay on loopback

Leave `[server] listen` at its default, `127.0.0.1:8443`, on the same host as
the proxy. Nothing but the proxy can then reach the relay, and loopback is the
only bind on which a single-tenant relay will start. A multi-tenant relay (an
OIDC issuer configured) may bind elsewhere — a private interface the proxy
reaches over a container network, say — but keep it off any public interface:
the proxy is where TLS, the body limit and `/metrics` are handled.

### 2. Tell the relay which proxy to believe

```toml
[server]
trusted_proxies = ["127.0.0.1"]   # the address the proxy connects from
```

The per-address rate limits key on the client's address. Behind a proxy the
socket peer is the proxy, so without this line **every user counts against the
proxy's one bucket**, and the relay's logs see one client. With it, the relay
reads `X-Forwarded-For` from that peer and takes the right-most address that
is not a listed proxy. A header from any other peer is ignored, so a client
talking to the relay directly cannot pick its own bucket.

`X-Forwarded-For` is the only field read. RFC 7239 `Forwarded` is ignored
from every peer, trusted or not: a proxy appends to one field and passes the
other through from the client, so believing both would let a client name its
own address through whichever one its proxy does not write. A proxy that only
writes `Forwarded` therefore cannot be trusted by this relay.

List every hop you operate: a CDN or load balancer in front of the proxy goes
in too, as CIDRs (`["127.0.0.1", "10.0.0.0/8"]`), and each hop must append to
`X-Forwarded-For` rather than replace it. A `/0` is refused, since it would
believe a forwarding header from any client. Both shipped files also drop a
client's own `Forwarded` header, which the relay ignores anyway, so nothing
behind the proxy is handed a field a client wrote.

### 3. Point the proxy at the relay

**Caddy.** Copy `deploy/caddy/Caddyfile` to `/etc/caddy/Caddyfile` and set
three environment variables for the Caddy service:

```sh
SUNRISE_DOMAIN=relay.example.com
SUNRISE_TLS=admin@example.com     # ACME account email; certificates are automatic
SUNRISE_UPSTREAM=127.0.0.1:8443   # optional; this is the default
```

**nginx.** With the official image, mount
`deploy/nginx/sunrise.conf.template` at
`/etc/nginx/templates/sunrise.conf.template` and set `SUNRISE_DOMAIN`,
`SUNRISE_UPSTREAM` and `SUNRISE_TLS_DIR` (the directory holding
`fullchain.pem` and `privkey.pem`, e.g. certbot's
`/etc/letsencrypt/live/relay.example.com`). Elsewhere, substitute the three
names by hand and include the file in your `http {}` block.

### What both configurations do, and why

| Property | Why |
|---|---|
| TLS terminates at the proxy; port 80 only redirects | Bearers never cross a network in plaintext |
| The client address goes upstream in `X-Forwarded-For`; a client's `Forwarded` is dropped | Step 2: the rate limits and the refusal log key on the real client |
| Body limit 2 MiB, the relay's `max_body_bytes` (which covers the 1 MiB blob chunk) | An oversize body is refused at the edge with the same `413` the relay would send. Change both together |
| `/api/v1/sync/events` is unbuffered | Sync events must reach the client as they happen; a buffering proxy holds them until its buffer fills |
| No timeout below an hour on a response in progress, idle connections closed after 5 min | The event stream is quiet between keep-alive comments every 15 s (`api::sync::KEEP_ALIVE_SECS`); a 30 s or 60 s proxy timeout would cut it |
| `/metrics` answers `404` at the proxy | The relay mounts `/metrics` on a loopback bind — exactly the bind behind the proxy — so the proxy is what keeps it private |

To scrape metrics, run the scraper on the relay's host against
`http://127.0.0.1:8443/metrics`, never through the proxy.

### Not yet wired

These appear in earlier drafts of this document and are **not implemented**;
the parser will reject them rather than accept them silently:
`[tls]` (terminate TLS at a reverse proxy for now), `[push]` providers other
than `[push.apns]`, `[quotas]`, `[storage] mode` / `sqlite_pool_size` / `postgres_url` /
`s3_*`, `[server] public_url`, and `[auth] oidc_client_secret` /
`admin_emails`. The `doctor` those drafts described is
`sunrise-server admin doctor` (see "Admin CLI"). The scaled deployment's keys
are `[storage] backend`, `postgres_url_file`, `[blobs] backend` and
`s3_credentials_file`, as
[ADR-0062](../11-adr/0062-relay-scale-out-storage-seam.md) §6 decides. All
are built (see "Several nodes over Postgres" and "Blobs on an object store").
The draft names above stay refused.

## Stopping, health and readiness

**Stopping.** `SIGTERM` or `SIGINT` starts a drain, logged as
`srv.stop.draining`:

1. `GET /api/v1/health?deep=1` starts answering `503`, and in the same
   instant the listener stops accepting and idle connections close. There is
   no pause between the two, so a load balancer sees refused connections
   rather than a `503`; only a probe already in flight when the signal lands
   gets the `503`. A balancer that must stop routing before the listener goes
   away needs the orchestrator to deregister the instance before it sends
   `SIGTERM` (a Kubernetes `preStop` sleep, for one).
2. Every open sync stream is sent a `closed` event with
   `SYNC_NETWORK_UNAVAILABLE`, which clients treat as retryable: they
   reconnect with backoff, to this relay once it is back or to another.
3. Requests already in flight — an ops batch, a blob chunk — get
   `[server] shutdown_grace_secs` (default 25) to finish.
4. The WAL is checkpointed into `sunrise.db`, `srv.stop` is logged with
   `result = "drained"`, and the process exits 0.

Requests still running at the deadline are cut, the WAL is still
checkpointed, `srv.stop` says `result = "timed_out"`, and the exit status is 1. A second signal during the
drain abandons it at once, also with status 1. Give the supervisor a stop
timeout longer than the grace period: Docker's default is 10 s
(`docker run --stop-timeout 30`, or `stop_grace_period: 30s` in Compose);
systemd's `TimeoutStopSec` and Kubernetes' `terminationGracePeriodSeconds`
both default to 30 s, which 25 s fits inside.

**Liveness and readiness.** Both are `GET /api/v1/health`, unauthenticated and
mounted on every listener, loopback or not:

| Probe | Answers `200` when | Use it for |
|---|---|---|
| `/api/v1/health` | the process is serving at all | liveness: restart when it fails |
| `/api/v1/health?deep=1` | not draining, the store answers within 2 s, the blob root is writable within 5 s, its filesystem has more than 5% free | readiness: route traffic only while it passes |

A failing readiness check answers `503` with `failed` naming the checks; see
[`api.md`](./api.md) §Health / meta. Do not wire `?deep=1` as a liveness
probe: a restart does not cure a wedged disk, and a drain is meant to fail it.

**Container health.** `sunrise-server healthcheck` probes the liveness route
on the address `[server] listen` names (a wildcard address is probed on
loopback) and exits 0 when it answers `200`, 1 otherwise; `--deep` probes
readiness instead. The OCI image's `HEALTHCHECK` runs it, so `docker ps` shows
`healthy` with no `curl` in the image. It resolves the config as the server
does, so a config passed only with `--config` on the server's command line is
not seen by the probe: put the path in `$SUNRISE_CONFIG`, or mount the file at
`/etc/sunrise/sunrise.toml`.

## Several nodes over Postgres

`[storage] backend = "postgres"` runs the relay as several identical nodes
behind one load balancer, all on one Postgres database
([ADR-0062](../11-adr/0062-relay-scale-out-storage-seam.md)). Any node serves
any account, and a node can stop at any time without its clients losing an op.
SQLite stays the default; nothing below applies to a single-binary relay.

```toml
[storage]
data_dir          = "/srv/sunrise"        # blobs/ under it, on a mount every node shares
backend           = "postgres"
postgres_url_file = "/etc/sunrise/pg.url" # postgres://sunrise:…@db.internal:5432/sunrise
postgres_ca_file  = "/etc/sunrise/pg-ca.pem"
```

```text
clients ──► load balancer ──► node 1 ─┐
                          ├─► node 2 ─┼─► Postgres (metadata, relay log, sessions, rate cells)
                          └─► node 3 ─┘
                                      └─► shared blob mount (data_dir/blobs)
```

**The connection.** `postgres_url_file` holds one Postgres URL or `key=value`
connection string, password included, so the relay refuses it unless only its
owner can read it (`chmod 600`); a config file or the environment is readable
by more than the process. With `postgres_ca_file` every connection is TLS and
the server's certificate must chain to the PEM roots in that file, and to
nothing else: no public CA is trusted by default, because a database
certificate is as often from a private CA. For a database whose certificate
comes from a public CA, point it at the system bundle
(`/etc/ssl/certs/ca-certificates.crt` on Debian). Without the file connections
are plaintext, and a URL asking for `sslmode=require` is refused rather than
quietly downgraded. Each node holds up to 16 pooled connections plus one for
its notice listener and, on the maintenance leader, one more.

**What the nodes share.** Accounts, devices, push tokens, cursors, tombstones,
the blob ledger, the pins, the durable relay log, sync sessions and rate
limits all live in Postgres:

- A session opened on one node is honoured on every other. Sessions are rows
  keyed by a hash of the id, so neither a read of the table nor a backup of it
  holds an id anyone could present.
- An op batch accepted on any node reaches every stream subscribed to its
  channel on every node: the append notifies `sunrise_relay` when it commits,
  and each node reads the frames it has not published yet out of the shared
  log. Each live event carries its log id, the same number on every node, so a
  client whose node stops resumes on another from its `Last-Event-ID` with no
  gap and nothing applied twice.
- A device revoked through one node is shut out by every node at once for new
  requests. A stream the device already holds on another node ends within
  `device_recheck_ms` (30 s by default): that is the bound, and lowering it
  costs one indexed read per open stream per interval.
- Rate limits are one budget per client across the cluster.
- The maintenance pass runs on one node at a time: whichever takes the
  maintenance lock for that pass. A node that dies releases it with its
  connection, and the next pass runs on another.

**What stays per node.** These count what one process holds, so a cluster's
ceiling is the per-node value times the node count:

- `[limits] streams` and `open_uploads`.
- The pairing rendezvous (`/api/v1/pairing/*`): both devices of one pairing
  must reach the same node, so route that prefix to one node or pin it with
  sticky sessions. Moving it into Postgres is #1262.
- Which devices are online, for push: a device streaming on one node can be
  sent a wake-up push by another. It wakes, finds its stream current, and
  sleeps again.
- `sunrise_sync_fanout_latency_seconds` is not observed on a Postgres node:
  it times the single-binary relay's in-process fan-out, which a cluster does
  not use.

**Blobs** stay on the filesystem under `data_dir/blobs`, which every node must
mount (NFS, a cluster filesystem), or move to an object store every node
reaches (see "Blobs on an object store"), which needs no `data_dir`.

**Migrations.** The database carries its own numbered history in a
`schema_version` table. Every node migrates on start, one at a time under an
advisory lock, so starting several new nodes together applies each step once.
A node refuses a database a newer release migrated and exits 78, as it does
for `sunrise.db`.

**Rolling restart.** Restart one node at a time; each stop is the drain
described in "Stopping, health and readiness":

1. Take the node out of the balancer's rotation, or let its readiness probe
   (`/api/v1/health?deep=1`) fail it out once `SIGTERM` lands.
2. `SIGTERM` it. Its streams end with a retryable `closed`, and their clients
   reconnect through the balancer to another node, which finds the session in
   the shared table and replays from the client's last event id.
3. Start it again, and wait for its readiness probe to pass before the next.

The first node of a newer release to start applies that release's
migrations, and the relay makes no promise that the older nodes still serving
keep working against the newer schema. Upgrade a cluster across a release
that adds a Postgres migration by stopping every node, then starting the new
ones; one that adds none rolls node by node.

**Admin CLI.** `stats`, `gc`, `account` and `device` work against Postgres.
`backup`, `rekey` and the doctor's `quick_check` are SQLite's and say so; back
Postgres up with its own tools (`pg_dump`, or base backups with WAL archiving)
and the blob mount with yours.

**Trying it locally.** A throwaway Postgres in Docker is enough for a
two-node cluster and for the backend's tests:

```sh
docker run -d --name sunrise-pg -e POSTGRES_PASSWORD=sunrise -p 127.0.0.1:5432:5432 postgres:18
SUNRISE_TEST_POSTGRES_URL=postgres://postgres:sunrise@127.0.0.1:5432/postgres \
  cargo test -p sunrise-server --lib store::postgres
docker rm -f sunrise-pg
```

Without `SUNRISE_TEST_POSTGRES_URL` those tests print why they did nothing and
pass; CI's `postgres` job runs them on every push to `master`.

## Blobs on an object store

`[blobs] backend = "s3"` keeps attachment ciphertext in one bucket of an
S3-compatible object store (AWS S3, MinIO, and others that speak path-style
requests and Signature Version 4) instead of under `data_dir/blobs`
([ADR-0062](../11-adr/0062-relay-scale-out-storage-seam.md) §7). The
filesystem stays the default.

```toml
[blobs]
backend             = "s3"
s3_endpoint         = "https://s3.us-east-1.amazonaws.com" # scheme, host, port; nothing after
s3_bucket           = "sunrise-blobs"                       # created beforehand
s3_region           = "us-east-1"                           # MinIO's default too
s3_credentials_file = "/etc/sunrise/s3.toml"
```

`s3_credentials_file` is TOML holding `access_key_id` and `secret_access_key`,
and the relay refuses it unless only its owner can read it (`chmod 600`), for
the reason `postgres_url_file` is. The key pair needs `GetObject`,
`PutObject`, `DeleteObject` and `ListBucket` on the bucket, nothing else. The
relay writes under `blobs/` and its readiness probe under `probes/`; give it a
bucket of its own. `http://` endpoints are accepted for a store on a private
network: what crosses it is ciphertext, and the signature never carries the
secret.

Each chunk is one object of at most 1 MiB, so no multipart upload is ever
started. Finalize re-reads and re-hashes every chunk and writes the manifest
last, as on the filesystem; the maintenance pass sweeps abandoned uploads,
orphaned account trees and collected blobs in the bucket. `admin backup`
copies the committed blobs into the backup's `blobs/` in the filesystem
layout, so a backup restores onto either backend. The readiness probe writes,
reads back and removes one object; there is no free-space check.

**Trying it locally.** A throwaway MinIO runs the backend's tests:

```sh
docker run -d --name sunrise-minio -p 127.0.0.1:9000:9000 alpine/minio:RELEASE.2025-10-15T17-29-55Z
SUNRISE_TEST_S3_ENDPOINT=http://127.0.0.1:9000 cargo test -p sunrise-server --lib blob::s3
docker rm -f sunrise-minio
```

Without `SUNRISE_TEST_S3_ENDPOINT` the store tests print why they did nothing
and pass; CI's `s3` job runs them on every push to `master`.

## Operator surfaces

| Surface | Purpose | Built |
|---|---|---|
| `/api/v1/health` (every listener) | Liveness, and readiness with `?deep=1` | yes — see "Stopping, health and readiness" |
| `sunrise-server admin <cmd>` (a shell on the host) | Doctor, stats, manual GC, account and device actions, backup | yes — see "Admin CLI" |
| `/api/v1/admin/*` | The same over HTTP | **no — nothing under `/api/v1/admin/` is routed**; the CLI is the admin surface |
| Prometheus `/metrics` | Aggregate metrics (no per-account labels with PII) | yes, at the router root — **mounted only on a loopback bind** |
| Logs (stderr) | Structured NDJSON; never contains content; never contains push tokens | yes (`sunrise_log::init_stderr`) |

**`/metrics` and any admin surface MUST be reachable on loopback only, or behind
operator authentication.** `build_service` enforces it through
`api::operator_surface` (`crates/sunrise-server/src/api/mod.rs`): the metrics
route is mounted only when `bind` is a loopback address, and a non-loopback bind
logs `srv.start.metrics_withheld` and serves `404` there instead. Read those two
symbols rather than the constructor alone — the check is in
`operator_surface`, and `build_service` is where it is wired in. A relay behind
the shipped proxy configurations binds loopback, so it *does* mount `/metrics`;
the proxy answers `404` for it, and scraping happens on the host (see "Reverse
proxy").

Logs go to **stderr**, not stdout: `main.rs` installs `sunrise_log::init_stderr()`
as its first statement, tuned by `SUNRISE_LOG` and `SUNRISE_LOG_FORMAT`.

## Admin CLI

```sh
sunrise-server admin [-c <path>] [--json] <command>
```

| Command | Does |
|---|---|
| `doctor` | The checks in "Testing the install" below; exits 1 if any fails |
| `stats` | Accounts (and how many are pending deletion), devices, push tokens, relay frames and bytes, tombstones, blob files and bytes, pending uploads |
| `gc --dry-run` / `gc --now` | Report, or run, one maintenance pass: erase deleted accounts past their grace period, collect tombstoned blobs, sweep abandoned uploads |
| `account list` / `account show <id>` | Account ids, creation and deletion times, device and tombstone counts |
| `account delete <id>` | Mark the account for deletion, as `DELETE /api/v1/accounts/me` does |
| `account delete <id> --immediately` | Erase it now: rows, relay log and blob trees |
| `device revoke <device_id>` | Revoke a device row, as `DELETE /api/v1/devices/<id>` does |
| `backup <dest_dir>` | Write `<dest_dir>/sunrise.db` through SQLite's online backup API (one consistent instant, under the database's own key, while the relay keeps writing) and copy `blobs/` beside it, manifests last |
| `rekey <new_key_file>` | Re-encrypt the database under the key in `new_key_file`, with the relay stopped (see "Encryption at rest") |

`--json` prints one JSON document; without it the same fields print as indented
`key: value` lines. Output names account and device ids and never an email or an
OIDC subject. Exit status: 0 done, 1 the command failed, 2 a usage error, 78 a
config that does not resolve or names no `[storage] data_dir`.

**It works on the data dir directly, not through the server.** It resolves the
config as the server does (`-c`, `$SUNRISE_CONFIG`, then the implicit paths)
and opens `sunrise.db` itself. No admin listener exists to expose, authenticate
or forget about, and the surface is reachable only by someone with a shell on
the host and read access to the data dir. It is safe beside a running relay:
the database is in WAL mode, every write is one short transaction, and each
process waits out the other's lock for `busy_timeout_ms`. The one thing a CLI
erasure cannot reach is the running server's memory. That is the retained ring
of the erased account's channels and its open streams. No client can address
either again, and both go at the server's next restart.

Run the `admin` of the release the server runs. Opening the store migrates it,
exactly as starting the server does, so a newer binary's `admin` upgrades the
schema under an older running server. The older server then refuses that file
at its next start (see "Upgrade").

The serving binary runs the same maintenance pass itself at startup and every
`maintenance_interval_secs`, so `gc --now` is for when an operator wants it
sooner.

## Encryption at rest

With `[storage] encrypt = true`, `sunrise.db` is SQLCipher-encrypted as a whole
under the 32-byte key in `key_file`
([ADR-0060](../11-adr/0060-relay-database-encryption-at-rest.md)). Without it
the file is plain SQLite, readable by anyone who can read the data dir, and it
holds account emails, OIDC subjects, device nicknames and push tokens (see
[`relay-and-blob-storage.md`](./relay-and-blob-storage.md)). It is off by
default, because the relay will not invent a key; turn it on for any
deployment that holds real accounts.

**Setting it up.** Create the key outside the data dir, readable by the user
the relay runs as and nobody else, then name it:

```sh
install -d -m 700 /etc/sunrise
openssl rand -hex 32 > /etc/sunrise/db.key
chmod 600 /etc/sunrise/db.key
```

**Back the key up apart from the data dir.** It is the only way into the
database and into every backup of it: lose it and both are unreadable. Keep it
in a password manager or a secrets store, never beside a backup.

**Turning it on for an existing install** is a one-way migration, run by the
first process that opens the database under the key (the relay, or any `admin`
command). It needs the file to itself: stop the relay, set the two keys, start
it. The relay exports every row into an encrypted file, keeps the original as
`sunrise.db.pre-encryption`, renames the encrypted file into place, and logs
`srv.store.encrypted`. A crash before the rename leaves the plaintext database,
and the next start retries. `admin doctor` fails its `encryption` check while
`sunrise.db.pre-encryption` exists: once the relay runs on the encrypted
database, delete it, and any backup that contains it. There is no way back to
plaintext.

**Rotating the key:**

1. Stop the relay.
2. Write the new key file: `openssl rand -hex 32 > /etc/sunrise/db.key.next`,
   then `chmod 600` it.
3. `sunrise-server admin rekey /etc/sunrise/db.key.next`. It refuses while any
   other process has the database open, and leaves the old key working if it
   fails.
4. Point `key_file` at the new file (or move it over the old one), and start.
5. Keep the old key for as long as you keep backups taken before the rotation:
   they are still under it.

## Backup

- **Online, the default:** `sunrise-server admin backup <dest_dir>` while the
  relay runs. The database is copied with SQLite's online backup API in one
  pass over a single snapshot, so the copy is one committed instant, and the
  relay's writes carry on throughout; the WAL grows by what is written during
  the copy and shrinks at the next checkpoint. An encrypted database is copied
  under the same key, so the backup is ciphertext and needs the same
  `key_file` to open. The blob tree is copied after the database, manifests
  last, so every blob in the backup is whole; a blob finalized after the
  database copy is left out, which is safe because no op in that copy names
  it.
- **Scheduled:** run the same command from a systemd timer or cron, into a new
  directory each time, and ship the directories off-host. For an encrypted
  install this is the continuous option: what leaves the host is ciphertext,
  and the key stays behind.
- **Continuous replication with Litestream**, for a plaintext install only.
  Litestream ships the WAL as it is written, and recovers to within seconds.
  Point it at `<data_dir>/sunrise.db`, keep `blobs/` on its own backup, and
  expect its `_litestream_seq` and `_litestream_lock` tables in the file,
  which the relay ignores. Litestream opens the database through its own
  SQLite build, which has no SQLCipher codec, so it cannot read an encrypted
  install; use the scheduled backup there.
- **Offline:** stop, `tar czf` the data dir, start. Or use a snapshot-aware
  filesystem (ZFS, Btrfs). The op log lives inside `sunrise.db`, so that one
  file plus `blobs/` is the entire server state. The database runs in WAL mode,
  so while the server runs its recent writes are in `sunrise.db-wal` beside it:
  a copy of `sunrise.db` alone, taken from a running server, can be missing
  them. A stop by `SIGTERM` checkpoints the WAL into `sunrise.db` once its
  drain ends; a snapshot must take the whole directory at one instant, which
  ZFS and Btrfs snapshots do.
- **What is in a backup.** Without encryption, everything the relay database
  holds, in plaintext: treat the backup accordingly. With it, ciphertext.
  Neither carries the key file or the APNs key, which are config.
- Several nodes over Postgres: Postgres's own backup tooling for the database,
  and yours for the shared blob mount. `admin backup` copies a SQLite database
  only.

### Restore

1. Stop the relay.
2. Move the data dir aside (keep it until the restore is verified).
3. Copy the backup's `sunrise.db` and `blobs/` into a fresh data dir, owned by
   the user the relay runs as. Leave out any `sunrise.db-wal` and
   `sunrise.db-shm`: an `admin backup` has none, and a stale one beside a
   restored file is not its WAL.
4. Keep `[storage] key_file` naming the key the backup was taken under, if it
   is encrypted.
5. Start the relay, and run `sunrise-server admin doctor`.

Clients whose cursors the backup covers resume from them with no gap. What
the relay accepted after the backup was taken is not in it: the interval
between backups is the relay history you accept losing.

## Upgrade

- Stop, back up (above), replace binary, start. The new binary migrates
  `sunrise.db` forward on its first start, logging one `srv.store.migrated`
  line per step applied.
- The schema is versioned. `sunrise.db` records its version in SQLite's
  `PRAGMA user_version`, and each release carries a numbered, append-only list
  of migrations (`crates/sunrise-server/src/store/migrations/mod.rs`). Each one
  runs in its own transaction together with its version stamp, so an upgrade
  interrupted by a crash or a power cut resumes from the last completed step on
  the next start. A database written before versioning existed is at version 0
  and adopts the history unchanged: migration 0001 is the schema those releases
  created, and creates nothing on a file that already has it.
- **Downgrade is refused, not attempted.** A binary started against a database
  a newer release migrated logs `srv.start.refused` naming both versions and
  exits 78 without writing a byte to the file. Run the newer binary again, or
  restore the backup taken before the upgrade.
- On every start the server also runs SQLite's `PRAGMA quick_check`, stopped
  after 10 s, and logs `srv.store.quick_check`. A result other than `ok` does
  not stop the server; it is the signal to restore a backup.
- A relay running as several nodes over Postgres restarts node by node, and
  upgrades that way across a release that adds no Postgres migration; see
  "Several nodes over Postgres" §Rolling restart.

## Testing the install

`sunrise-server admin doctor` runs the checks that apply to a single-binary
SQLite relay, reports each as `ok`, `fail` or `skipped` with a reason, and exits
1 if any failed:

| Check | What it does |
|---|---|
| `config` | Validates the config against every refusal in "Refusals" above |
| `database` | `PRAGMA quick_check` on `sunrise.db` (opening it also refuses a newer schema) |
| `storage` | Writes 10 MiB under the data dir, fsyncs, reads it back byte-for-byte, and deletes it |
| `blob_root` | The blob root is writable |
| `push` | `[push.apns]` is configured, and its key loaded and signs; `skipped` without `[push]` |
| `protocol` | The wire, document-schema and crypto-suite versions this binary speaks |
| `free_space` | The filesystem holding the blob root has more than 5% of its blocks available, the same reading as readiness's `disk`; `skipped` where there is no local filesystem to read |
| `tls` | Always `skipped`: the relay serves plain HTTP behind the reverse proxy |

Two checks earlier drafts listed are not here. Postgres `fsync = on` has no
Postgres to ask. A synthetic op round-trip needs a client that seals
envelopes, and the server deliberately has no `sunrise-crypto` dependency. Log
retention is the log collector's, since the relay writes to stderr and keeps
nothing.

## What self-hosters give up vs managed cloud

| Feature | Managed | Self-host |
|---|---|---|
| Push reliability | High (Sunrise-operated APNs/FCM) | Operator-managed; optional. APNs only, with the operator's own `.p8` key, so it wakes only an app build whose bundle id is the configured `topic`; FCM and Web Push are not built — see [`push-notifications.md`](./push-notifications.md). |
| Cross-server sharing | n/a (same-server only) | n/a |
| Capacity scaling | Auto | Operator-driven |
| Backups | Sunrise-managed | Operator-managed |

Functional features (E2EE, sync, multi-client, sharing within the same server) are **identical**. Sharing is not implemented on either (see [`api.md`](./api.md) §Sharing).

## Calendar OAuth clients

A self-hosted relay needs no Google or Microsoft registration, and
`sunrise.toml` has no setting for one. Each device authorizes against the
calendar provider itself and keeps its token in its own keychain; the relay
only carries the sealed account configuration and events
([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)). An
official app build connects Google and Microsoft the same way whichever relay
it syncs through.

The OAuth client ids are inputs to the **app** build. Building the apps from
source with your own registrations, for example under your own bundle id, is
described in [`google-calendar.md`](../09-integrations/google-calendar.md)
§Client registration and
[`microsoft-graph.md`](../09-integrations/microsoft-graph.md) §App
registration.

## Migrations between topologies

**Not implemented.** There is no managed cloud to migrate from, and step 3's
"re-pair against a new server, uploading its full op log" has no client
implementation. The intended flow:

A managed-cloud user can move to self-host:

1. Spin up self-host.
2. Add the new server URL to a trusted device's settings.
3. The device "re-pairs" against the new server, uploading its full op log.
4. Other devices update their server URL setting; they re-sync.
5. Cancel managed account; managed server purges data.

The user's identity and data are unchanged (clients are authoritative).
