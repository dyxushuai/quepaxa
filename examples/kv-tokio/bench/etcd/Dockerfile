# One image for the comparison: kv-tokio and kv-bench, etcd's benchmark tool, and the
# network tools the fault sidecar uses. etcd itself runs from its official image.
# Build from the repository root: docker build -f examples/kv-tokio/bench/etcd/Dockerfile .

FROM rust:1.91-bookworm@sha256:c1e5f19e773b7878c3f7a805dd00a495e747acbdc76fb2337a4ebf0418896b33 AS rust
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p kv-tokio --bins

FROM golang:1.26.9-bookworm@sha256:d9c68c2c51161e12fd77e4c6320687c9cd86e1af1e3ad6e6cd63ff970641453c AS go
ARG ETCD_VERSION=v3.7.2
RUN git clone --depth 1 -b "$ETCD_VERSION" https://github.com/etcd-io/etcd /etcd \
    && cd /etcd && CGO_ENABLED=0 go build -o /benchmark ./tools/benchmark

FROM debian:bookworm-20261005-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN apt-get update && apt-get install -y --no-install-recommends iproute2 iptables \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust /src/target/release/kv-tokio /src/target/release/kv-bench /usr/local/bin/
COPY --from=go /benchmark /usr/local/bin/benchmark
