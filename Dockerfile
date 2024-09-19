FROM rust:bookworm AS builder
WORKDIR /app

ENV DEBIAN_FRONTEND=noninteractive

# not pinning apt package versions yet
# hadolint ignore=DL3008
RUN apt-get update && \
apt-get install --no-install-recommends --yes \
    pkg-config \
    libssl-dev \
    ca-certificates && \
rm -rf /var/lib/apt/lists/* 

COPY . .

RUN cargo build --release --bin log_reporter 

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# not pinning apt package versions yet:
# hadolint ignore=DL3008
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install --no-install-recommends --yes \
        libssl-dev \
        ca-certificates && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/log_reporter /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/log_reporter"]
