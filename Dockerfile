# Stage 1: Build
FROM rust:1.94-slim AS builder

WORKDIR /build

# Install build dependencies
# pip cmake provides 3.26+ needed by libaec-sys (Bookworm ships 3.25)
# clang needed for libaec-sys (GRIB CCSDS compression)
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev clang python3-pip \
    && pip install --break-system-packages cmake \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/engine-csv/Cargo.toml crates/engine-csv/Cargo.toml
COPY crates/engine-geojson/Cargo.toml crates/engine-geojson/Cargo.toml
COPY crates/engine-geotiff/Cargo.toml crates/engine-geotiff/Cargo.toml
COPY crates/engine-querydata/Cargo.toml crates/engine-querydata/Cargo.toml
COPY crates/engine-grib/Cargo.toml crates/engine-grib/Cargo.toml
COPY crates/storage/Cargo.toml crates/storage/Cargo.toml
COPY crates/render/Cargo.toml crates/render/Cargo.toml
COPY crates/api-edr/Cargo.toml crates/api-edr/Cargo.toml
COPY crates/api-features/Cargo.toml crates/api-features/Cargo.toml
COPY crates/api-wms/Cargo.toml crates/api-wms/Cargo.toml
COPY crates/api-maps/Cargo.toml crates/api-maps/Cargo.toml
COPY crates/api-tiles/Cargo.toml crates/api-tiles/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml

# Create stub lib.rs files so cargo can resolve the workspace and cache deps
RUN for dir in core engine-csv engine-geojson engine-geotiff engine-querydata engine-grib storage render api-edr api-features api-wms api-maps api-tiles; do \
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
