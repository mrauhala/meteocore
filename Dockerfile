# syntax=docker/dockerfile:1.7
# Stage 1: Build
FROM rust:1.94-slim AS builder

WORKDIR /build

# Build dependencies:
#  - `pkg-config`, `libssl-dev`, `clang` — needed by libaec-sys (GRIB CCSDS
#    compression). `clang` provides libclang for bindgen.
#  - `cmake` from `bookworm-backports` (3.31) — libaec-sys requires ≥3.26,
#    and Bookworm's main repo ships 3.25. Pulling from backports avoids the
#    previous `pip install --break-system-packages cmake` hack.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    echo 'deb http://deb.debian.org/debian bookworm-backports main' \
        > /etc/apt/sources.list.d/backports.list && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config libssl-dev clang && \
    apt-get install -y --no-install-recommends -t bookworm-backports cmake && \
    rm -rf /var/lib/apt/lists/*

# Copy the full workspace in one shot. The dep-cache layer split that used
# to live here (manifest-only COPYs + stub lib.rs files + a throwaway build)
# was always brittle — adding a new crate required updating the Dockerfile
# in lock-step with `Cargo.toml`, and the cache only helped when Cargo.lock
# was unchanged. The BuildKit cache mounts below get the same benefit
# without enumerating workspace members, and they GC stale incremental
# artifacts automatically.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY schemas/ schemas/

# `--mount=type=cache` keeps `target/` and the Cargo registry on the
# BuildKit daemon between image builds. A typical source-only change
# now compiles just the changed workspace member instead of re-doing all
# 13 dependency crates from scratch. The final `cp` is required because
# the cache mount is wiped from the produced image (build-time only), so
# we copy the binary out to a stable path before the cache is detached.
RUN --mount=type=cache,target=/build/target,id=meteocore-target \
    --mount=type=cache,target=/usr/local/cargo/registry,id=meteocore-cargo-registry \
    --mount=type=cache,target=/usr/local/cargo/git,id=meteocore-cargo-git \
    cargo build --release -p server && \
    cp target/release/server /usr/local/bin/server-build

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

RUN groupadd -r dataserver && useradd -r -g dataserver dataserver

COPY --from=builder /usr/local/bin/server-build /usr/local/bin/dataserver

WORKDIR /data
COPY config.toml ./

USER dataserver

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8000/health || exit 1

CMD ["dataserver"]
