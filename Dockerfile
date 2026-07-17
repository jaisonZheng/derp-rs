FROM rust:1.94-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/derper-rs derper-rs \
    && install -d -o derper-rs -g derper-rs /var/lib/derper-rs
COPY --from=builder /src/target/release/derper-rs /usr/local/bin/derper-rs
USER derper-rs
WORKDIR /var/lib/derper-rs
EXPOSE 3340/tcp 3478/udp
ENTRYPOINT ["derper-rs"]
CMD ["--addr", "0.0.0.0:3340", "--stun-addr", "0.0.0.0:3478", "--private-key", "/var/lib/derper-rs/derper.key"]
