# Build stage: the same toolchain the rust-toolchain.toml pins, so the image
# is built by the compiler CI and developers use.
FROM rust:1.98.0-slim-trixie AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY openapi.json ./
# The GUI and the contract are embedded at compile time (rust-embed and
# include_str!), so the release binary is the entire application.
# --locked (#215): the release image is reproducibly pinned to the
# committed Cargo.lock; resolver drift breaks the build loudly instead of
# shipping a different dependency graph.
RUN cargo build --locked --release

# Runtime stage: no toolchain, no build files — only the binary and a
# data mount point.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get upgrade --yes \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin tucano \
    && mkdir -p /data \
    && chown tucano:tucano /data

COPY --from=builder /build/target/release/tucano-time /usr/local/bin/tucano-time

USER tucano
ENV TUCANO_DATA_DIR=/data
ENV TUCANO_PORT=8080
EXPOSE 8080
VOLUME ["/data"]

# Container liveness (#94): the same binary probes its own /healthz, so the
# runtime image needs no curl/wget.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/tucano-time", "--health"]

ENTRYPOINT ["/usr/local/bin/tucano-time"]
