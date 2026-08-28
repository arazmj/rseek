FROM rust:1-slim AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch --locked

COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/arazmj/rseek"
LABEL org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && useradd -m -u 1000 -s /bin/bash rseek \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rseek /usr/local/bin/rseek

USER rseek
WORKDIR /home/rseek
ENTRYPOINT ["/usr/local/bin/rseek"]
