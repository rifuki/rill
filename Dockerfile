# Multi-stage: build with the full toolchain, ship a binary and nothing else.
#
# The Bun-based image this replaces carried an entire JavaScript runtime and the monorepo's
# node_modules, and needed the repository root as build context because the backend imported a
# sibling package by relative path. A static binary needs neither.

FROM rust:1.96-slim AS builder
WORKDIR /build

# protoc is needed to build the Sui gRPC bindings; pkg-config and libssl for the TLS stack.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# Manifests first, so a source-only change does not re-download and re-build every dependency.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY bins ./bins
COPY fixtures ./fixtures

RUN cargo build --release --bin rill-server

# ── runtime ──
FROM debian:bookworm-slim AS runtime
WORKDIR /app

# ca-certificates for outbound TLS to the Sui fullnode; curl only so the healthcheck below works.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# Never run as root. The data directory is a mounted volume, so it is chowned rather than copied.
RUN useradd --system --create-home --uid 10001 rill
COPY --from=builder /build/target/release/rill-server /usr/local/bin/rill-server
RUN mkdir -p /app/data && chown -R rill:rill /app
USER rill

# The port and healthcheck path are part of the deployment contract — Dokploy and the existing
# compose configuration both depend on them, so they are unchanged from the Bun image.
ENV PORT=3939
EXPOSE 3939
HEALTHCHECK --interval=15s --timeout=5s --retries=3 --start-period=20s \
  CMD curl -f http://localhost:3939/health || exit 1

# `data/` holds skills.json and oauth.json and must be a volume: it is deliberately excluded from
# the image, and a single replica owns it — two would each hold half the authorization codes.
VOLUME ["/app/data"]

CMD ["rill-server"]
