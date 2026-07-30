# syntax=docker/dockerfile:1

# polyarb — Polymarket arbitrage scanner.
#
# Phase A is read-only: the binary discovers markets, fetches order books, detects
# mispricings and writes them down. It never signs anything, never places an order, and
# needs no wallet or API credentials. The only secrets it can use are the optional
# Telegram alert credentials, which arrive as environment variables at run time and are
# never baked into a layer.
#
# Build:  docker build -t polyarb:local .
# Run:    see docker-compose.yml / docs/deploy.md


# ===================================================================================
# Stage 1 — build
# ===================================================================================
# Pinned to the same Debian release as the runtime stage. That is not cosmetic: the
# binary links dynamically against glibc (2.34 symbols, verified with `objdump -T`), so
# building on bookworm and running on bookworm-slim guarantees there is no glibc version
# skew. `rusqlite` is compiled with the `bundled` feature, so SQLite itself is built from
# C source and statically linked — that needs a C toolchain, which the official rust
# image already provides.
FROM rust:1.94-bookworm AS builder

WORKDIR /src

# --- dependency layer --------------------------------------------------------------
# Compile the whole dependency graph against a stub `main` first. This layer's cache key
# is Cargo.toml + Cargo.lock only, so editing src/ no longer rebuilds ring, the bundled
# SQLite and clap — which are what make a cold build take ~2 minutes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

# --- application layer -------------------------------------------------------------
COPY src ./src

# `COPY` preserves the source files' mtimes, and cargo decides staleness by mtime — a
# checkout whose files are older than the stub build would be considered fresh and the
# stub binary would ship. Deleting the stub's artefacts and touching the real sources
# makes the rebuild unconditional; the final `test -x` refuses to produce an image at all
# if, despite that, no binary was emitted.
RUN touch src/*.rs \
 && rm -f target/release/polyarb target/release/deps/polyarb-* \
 && cargo build --release --locked \
 && test -x target/release/polyarb


# ===================================================================================
# Stage 2 — runtime
# ===================================================================================
# debian:bookworm-slim rather than distroless, deliberately:
#
#   * The image needs a shell. The compose healthcheck reads the SQLite file's mtime with
#     `stat`/`date`, and the whole point of a week-long unattended soak is that the owner
#     can `docker compose exec` in and look around when something seems off. Distroless
#     has neither a shell nor coreutils, so both would have to go.
#   * The cost is ~30 MB more image, once, on a VPS that exists to run one process for a
#     week. That is not a trade worth making against operability.
#   * CA certificates are *not* the reason. reqwest here is built with the
#     `rustls-tls-webpki-roots` feature (confirmed with `cargo tree -i webpki-roots`),
#     which compiles the Mozilla root store into the binary — TLS to Polymarket and to
#     the Telegram Bot API works on a totally bare filesystem. They are installed below
#     anyway, cheaply, so that a later switch to native roots or a debugging session
#     behind a TLS-inspecting proxy does not turn into a silent handshake failure.
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="polyarb" \
      org.opencontainers.image.description="Polymarket arbitrage scanner — dry-run only, never places orders."

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged. The scanner is an HTTP client that appends to three directories; nothing
# it does wants root. The uid is pinned to 10001 rather than left to useradd, because the
# host directories bind-mounted in docker-compose.yml have to be chowned to a number the
# deployment document can state (docs/deploy.md, step 6).
RUN groupadd --system --gid 10001 polyarb \
 && useradd --system --uid 10001 --gid 10001 --home-dir /app --shell /usr/sbin/nologin polyarb

WORKDIR /app

COPY --from=builder /src/target/release/polyarb /usr/local/bin/polyarb

# Baked in so `docker run polyarb:local scan` works against no host files at all. To
# change settings without rebuilding, mount your own file over
# /app/config/default.toml (see docker-compose.yml) or set $POLYARB_CONFIG.
# There are no secrets in this file and there never will be.
COPY config/ /app/config/

# config/default.toml addresses storage by paths relative to the working directory:
# data/polyarb.sqlite, logs/, reports/. Created and owned here so the container starts
# clean even with no mounts.
RUN mkdir -p /app/data /app/logs /app/reports \
 && chown -R 10001:10001 /app

USER 10001:10001

# Declared after the chown, so an anonymous volume created by a bare `docker run` is
# seeded with the right ownership. compose bind-mounts over all three, in which case the
# *host* directory's ownership applies instead — hence the chown step in docs/deploy.md.
VOLUME ["/app/data", "/app/logs", "/app/reports"]

ENV RUST_LOG=polyarb=info

# Documentation only (compose publishes the port explicitly, on the host's loopback): the
# M7 dashboard listens here when the image is run with `dashboard` instead of `run`. The
# daemon itself listens on nothing at all.
EXPOSE 8080

# Exec form: the daemon becomes PID 1 and receives SIGTERM directly, which it handles by
# draining the in-flight lifecycle re-polls before exiting.
ENTRYPOINT ["polyarb"]
CMD ["run"]
