FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json
# Build application
COPY . .
RUN cargo build --release --bin log_reporter

# We do not need the Rust toolchain to run the binary!
FROM debian:bullseye-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && \
    apt install -y \
        libssl-dev \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN mkdir -p /app/bin
WORKDIR /app
COPY --from=builder /app/target/release/log_reporter /app/
COPY --from=builder /app/bin/release.sh /app/bin/
ENTRYPOINT ["/app/log_reporter"]
