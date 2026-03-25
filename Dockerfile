# Stage 1: Build
FROM rust:1.87-slim AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/engine-csv/Cargo.toml crates/engine-csv/Cargo.toml
COPY crates/engine-geojson/Cargo.toml crates/engine-geojson/Cargo.toml
COPY crates/engine-geotiff/Cargo.toml crates/engine-geotiff/Cargo.toml
COPY crates/storage/Cargo.toml crates/storage/Cargo.toml
COPY crates/api-edr/Cargo.toml crates/api-edr/Cargo.toml
COPY crates/api-features/Cargo.toml crates/api-features/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml

# Create stub lib.rs files so cargo can resolve the workspace and cache deps
RUN for dir in core engine-csv engine-geojson engine-geotiff storage api-edr api-features; do \
      mkdir -p crates/$dir/src && echo "" > crates/$dir/src/lib.rs; \
    done && \
    mkdir -p crates/server/src && echo "fn main() {}" > crates/server/src/main.rs

RUN cargo build --release -p server 2>/dev/null || true

# Copy actual source and build
COPY crates/ crates/
COPY schemas/ schemas/
RUN cargo build --release -p server

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

RUN groupadd -r dataserver && useradd -r -g dataserver dataserver

COPY --from=builder /build/target/release/server /usr/local/bin/dataserver

WORKDIR /data
COPY config.toml ./

USER dataserver

EXPOSE 8000

CMD ["dataserver"]
