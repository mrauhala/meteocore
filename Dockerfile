# Stage 1a: cargo-chef shared base
#
# `cargo-chef` produces a stable "recipe" (Cargo.toml/lock-derived) so the
# dep-cache layer only invalidates when *dependencies* change, not when
# workspace source changes. Replaces the previous hand-enumerated stub-lib
# trick (which silently omitted `engine-postgis` when the workspace grew)
# with a self-maintaining mechanism that picks up new crates automatically.
FROM rust:1.94-slim AS chef
WORKDIR /build
RUN cargo install cargo-chef --locked --version 0.1.71

# Stage 1b: derive recipe.json from the full workspace
#
# `cargo chef prepare` only reads Cargo.toml + Cargo.lock, so recipe.json is
# stable across source-only changes — the next stage's cache hits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 1c: builder
FROM chef AS builder
#
# System dependencies:
#  - `pkg-config`, `libssl-dev`, `clang` from bookworm main — needed by
#    libaec-sys (GRIB CCSDS compression). `clang` provides libclang for
#    bindgen.
#  - `cmake` from `bookworm-backports` (3.31) — libaec-sys requires ≥3.26,
#    and Bookworm's main repo ships 3.25. Replaces the previous
#    `pip install --break-system-packages cmake` hack.
#
# `--no-install-recommends` is intentionally *omitted* from the backports
# `cmake` line: apt's solver needs the Recommends headroom to resolve
# transitive deps like `libjsoncpp25` correctly across the main/backports
# boundary. With `--no-install-recommends` the solver hits "unmet
# dependencies" on `libjsoncpp25` even though the package is available.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    echo 'deb http://deb.debian.org/debian bookworm-backports main' \
        > /etc/apt/sources.list.d/backports.list && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config libssl-dev clang && \
    apt-get install -y -t bookworm-backports cmake && \
    rm -rf /var/lib/apt/lists/*

# Cook the dep tree from the recipe. This is the layer that benefits most
# from cross-build caching — input is just `recipe.json` (manifests only),
# so it stays cache-valid until any Cargo.toml or Cargo.lock changes.
# Source changes do *not* invalidate it. `type=gha,mode=max` on the
# workflow's `cache-to` exports this layer.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p server

# Now copy the actual workspace source and build. Only the changed
# workspace member's crate needs to recompile; deps are already linked.
COPY . .
RUN cargo build --release -p server

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

RUN groupadd -r dataserver && useradd -r -g dataserver dataserver

COPY --from=builder /build/target/release/server /usr/local/bin/dataserver

WORKDIR /data
COPY config.toml ./

USER dataserver

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8000/health || exit 1

CMD ["dataserver"]
