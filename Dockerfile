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
# NB: `cargo chef prepare` (0.1.71) takes no `--features` — the recipe captures
# the full manifest incl. the *optional* `icechunk` dep, so the builder's
# `cook --features icechunk` pre-warms those crates from it. (Newer cargo-chef
# adds `prepare --features`; not needed / not supported here.)
RUN cargo chef prepare --recipe-path recipe.json

# Stage 1c: builder
FROM chef AS builder
#
# System dependencies:
#  - `pkg-config`, `libssl-dev`, `clang` — needed by libaec-sys (GRIB
#    CCSDS compression). `clang` provides libclang for bindgen.
#  - `make` — pip-installed `cmake` ships a static binary but still
#    drives `make` for its Unix Makefiles generator. Without an explicit
#    `make` install, libaec-sys's build script fails with "CMake was
#    unable to find a build program corresponding to 'Unix Makefiles'".
#    `clang` Recommends `make` but `--no-install-recommends` blocks that.
#  - `cmake` via pip — libaec-sys requires cmake ≥3.26, but Bookworm
#    main ships 3.25. The bookworm-backports route (cmake 3.31) was
#    tried and abandoned: apt's solver fails to resolve `libjsoncpp25`
#    across the main/backports boundary on current `rust:1.94-slim` even
#    though the package is in main. `pip install --break-system-packages
#    cmake` ships a self-contained static binary that sidesteps the
#    system-package solver entirely. PEP 668 makes Debian's Python
#    "system-managed" — `--break-system-packages` is the documented way
#    to opt out for build-stage tooling. Build-stage only; never lands
#    in the runtime image.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev clang make python3-pip ca-certificates && \
    pip install --break-system-packages cmake && \
    rm -rf /var/lib/apt/lists/*

# Cook the dep tree from the recipe. This is the layer that benefits most
# from cross-build caching — input is just `recipe.json` (manifests only),
# so it stays cache-valid until any Cargo.toml or Cargo.lock changes.
# Source changes do *not* invalidate it. `type=gha,mode=max` on the
# workflow's `cache-to` exports this layer.
#
# Cargo features compiled into the server. Defaults to `icechunk` (read
# transactional/versioned Icechunk Zarr repos; ~+8 MB, object_store S3 backend —
# no AWS SDK, #339). Build a leaner image without it via
# `--build-arg CARGO_FEATURES=""`, or pass a custom set. Threaded through BOTH
# `cook` and `build` so the cooked dep cache matches the final build.
ARG CARGO_FEATURES="icechunk"
COPY --from=planner /build/recipe.json recipe.json
# `--locked`: build strictly against the committed Cargo.lock (matches the
# release build) — important now that the default feature set pulls rev-pinned
# `[patch.crates-io]` git forks; no silent re-resolution in the shipped image.
RUN cargo chef cook --release --locked --recipe-path recipe.json -p server \
        ${CARGO_FEATURES:+--features "$CARGO_FEATURES"}

# Now copy the actual workspace source and build. Only the changed
# workspace member's crate needs to recompile; deps are already linked.
COPY . .
RUN cargo build --release --locked -p server \
        ${CARGO_FEATURES:+--features "$CARGO_FEATURES"}

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
