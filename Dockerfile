FROM rust:1.74 AS builder
WORKDIR /usr/src/relayer
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release


FROM ubuntu:22.04
WORKDIR /relayer-app
RUN apt-get update && apt-get install -y \
    openssl \
    ca-certificates \
    jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/relayer/target/release/relayer .

ENTRYPOINT ["/relayer-app/relayer", "--config", "config.toml"]
