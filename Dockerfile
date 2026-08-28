# syntax=docker/dockerfile:1.7
#
# `sunrise-server` — the self-host sync relay, as an OCI image.
#
# docs/06-server/self-hosting.md names the distribution artifacts: a single
# binary, an image at `ghcr.io/<org>/sunrise-server:<version>`, and one config
# file. This builds the second from the first.
#
#   docker build -t sunrise-server .
#   docker run --rm -v ./sunrise.toml:/etc/sunrise/sunrise.toml:ro \
#              -v sunrise-data:/var/lib/sunrise -p 8443:8443 sunrise-server
#
# ## The config is not optional in practice
#
# With no config the server falls back to its defaults, which bind
# `127.0.0.1:8443` — inside the container's own network namespace, reachable by
# nothing. Binding a routable address is refused (exit 78) while the server is
# single-tenant, because that would publish one shared account namespace to the
# network; see `ServerConfig::validate` and self-hosting.md §Refusals. So a
# useful container is either
#
#   * multi-tenant — `[auth] oidc_issuer` + `oidc_client_id` set, then
#     `[server] listen = "0.0.0.0:8443"` is accepted; or
#   * single-tenant behind a proxy that shares the namespace (`--network
#     container:`, a sidecar in the same pod).
#
# That refusal is the server's designed posture. The image does not paper over
# it with a baked-in config, because a default config file would be exactly the
# "server nobody asked for" that self-hosting.md refuses to start.

# Must match `rust-toolchain.toml`. The pin there is a reproducibility floor,
# and an image that silently built on something else would defeat it.
ARG RUST_VERSION=1.88.0
ARG DEBIAN_SUITE=bookworm

# ---------------------------------------------------------------------------
# Stage 1: build
# ---------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS builder

# `rusqlite/bundled-sqlcipher` compiles the SQLCipher amalgamation from source.
# Read libsqlite3-sys 0.28's build.rs: with no `OPENSSL_DIR` in the environment
# and a non-Apple target it takes the last branch — `-DSQLITE_HAS_CODEC` plus a
# bare `cargo:rustc-link-lib=dylib=crypto`, with no include path added. So the
# C compile needs `openssl/*.h` on the default search path and the link needs
# `libcrypto.so`; `libssl-dev` is what supplies both. (On macOS the same
# feature takes the `is_apple` branch and uses CommonCrypto, which is why a
# host build needs none of this.)
#
# pkg-config is deliberately *not* installed. Nothing on this path consults it,
# and installing it would let zstd-sys discover a system libzstd and link it
# dynamically — turning a statically bundled dependency into a runtime one
# depending on what happens to be in the builder image.
RUN apt-get update \
    && apt-get install --no-install-recommends -y libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Only what the workspace needs to compile. `.dockerignore` is an allowlist, so
# this cannot quietly pick up `target/` or `node_modules/`.
#
# `.cargo/config.toml` is *not* copied. Everything in it is an empty
# `[target.*]` placeholder except `[net] git-fetch-with-cli = true`, which needs
# a `git` binary the slim image does not carry — and needs it for nothing: the
# lock file has no git dependencies and cargo 1.88 reads the crates.io index
# over sparse HTTP. Copying it would mean installing git to satisfy a setting
# that changes no behaviour here.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# `--locked` because the image must build the dependency set the lockfile
# pins, and a resolver that is free to move makes the tag meaningless.
#
# The cache mounts are keyed by (id, target) so a cross-arch matrix does not
# have two architectures fighting over one `target/`. The binary is copied out
# inside the same RUN: a cache mount is not part of the layer, so anything left
# in `target/` afterwards is unreachable from later stages.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-target,target=/src/target,sharing=locked \
    cargo build --locked --release -p sunrise-server --bin sunrise-server \
    && install -Dm0755 target/release/sunrise-server /out/sunrise-server

# ---------------------------------------------------------------------------
# Stage 2: runtime
# ---------------------------------------------------------------------------
FROM debian:${DEBIAN_SUITE}-slim AS runtime

# - libssl3      supplies libcrypto.so.3, the `-lcrypto` the SQLCipher codec
#                links against (see the builder stage). Not optional: without
#                it the binary does not start.
# - ca-certificates  OIDC discovery and JWKS fetches are HTTPS
# - tzdata       jiff resolves zoned times against the IANA database on disk;
#                without it every zoned calculation fails at runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates libssl3 tzdata \
    && rm -rf /var/lib/apt/lists/*

# Non-root. A fixed high uid rather than an allocated one so a bind-mounted
# host data directory can be chowned to a number the operator can predict.
RUN groupadd --system --gid 10001 sunrise \
    && useradd --system --uid 10001 --gid sunrise --home-dir /var/lib/sunrise \
        --shell /usr/sbin/nologin sunrise \
    && install -d -o sunrise -g sunrise -m 0750 /var/lib/sunrise \
    && install -d -o root -g root -m 0755 /etc/sunrise

COPY --from=builder /out/sunrise-server /usr/local/bin/sunrise-server

# `/etc/sunrise/sunrise.toml` is the last implicit config path the server
# probes, so mounting a file there needs no flag and no env var.
#
# `VOLUME` keeps the database off the container's writable layer and marks the
# path an operator has to think about. The cost is that a `docker run` with no
# `-v` gets a fresh anonymous volume each time, so `[storage] data_dir` should
# be pointed here *and* the path bound to named storage.
VOLUME ["/var/lib/sunrise"]
EXPOSE 8443

# No HEALTHCHECK. There is no curl or wget in this image to write one with, and
# the admin health endpoint is loopback-only by default — an orchestrator
# should probe the relay's own port from outside instead.

USER sunrise:sunrise
WORKDIR /var/lib/sunrise

# `ENTRYPOINT` rather than `CMD` so `docker run <image> --config /elsewhere`
# appends flags instead of replacing the binary.
ENTRYPOINT ["/usr/local/bin/sunrise-server"]

# Set from the release workflow; repeated here so a local `docker build`
# produces a labelled image too. `image.source` is what links a GHCR package
# back to this repository.
ARG VERSION=0.0.0-dev
ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="sunrise-server" \
      org.opencontainers.image.description="Sunrise sync relay (REST + WebSocket, OIDC, self-host SQLite)" \
      org.opencontainers.image.source="https://github.com/justin13888/Sunrise" \
      org.opencontainers.image.documentation="https://github.com/justin13888/Sunrise/blob/master/docs/06-server/self-hosting.md" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}"
