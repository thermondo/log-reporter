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

# explanations for various mounts:
#
# * type=bind,target=.,rw: mounts the host's build context (the repository root) into our build
#   container with read/write privileges. WRITES WILL BE DISCARDED.
# * type=cache,target=...,sharing=private: tells docker to cache files between builds. not only does
#   this cache our dependencies, but also allows rust to do incremental builds rather than compiling
#   everything from scratch every time. sharing=private makes sure concurrent docker builds won't
#   step on each others' toes.
#
RUN --mount=type=bind,target=.,rw \
    --mount=type=cache,target=./target,sharing=private \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=private \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=private \
    cargo build --release --bin log_reporter && \
    # move build artifact elsewhere because all our writes to this directory will be discarded \
    mv target/release/log_reporter /

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

COPY --from=builder /log_reporter /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/log_reporter"]
