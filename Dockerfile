FROM rust:1.68.1 AS builder
WORKDIR /tmp/

# this build step will cache your dependencies
COPY Cargo.lock ./
RUN echo '[workspace]\nmembers = ["relayer"]' > Cargo.toml
COPY ./Cargo.toml relayer/Cargo.toml
RUN mkdir relayer/src && echo 'fn main() {}' > relayer/src/main.rs cargo build --release && rm -r relayer/src

# copy your source tree
COPY ./src ./relayer/src

# build for release
RUN cargo build --release

FROM ubuntu:20.04
WORKDIR /relayer-app
RUN apt update && apt install -yy openssl ca-certificates jq
COPY --from=builder /tmp/target/release/relayer .
ENTRYPOINT ["/relayer-app/relayer", "--config", "config.toml"]
