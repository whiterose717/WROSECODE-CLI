FROM rust:1.90-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash binutils file git ripgrep fd-find python3 gdb curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN ln -s /usr/bin/fdfind /usr/local/bin/fd
COPY --from=builder /src/target/release/wrosecode /usr/local/bin/wrosecode
WORKDIR /workspace
ENTRYPOINT ["wrosecode"]
