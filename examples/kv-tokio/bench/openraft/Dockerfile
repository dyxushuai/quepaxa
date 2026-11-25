# Standalone image for the Openraft KV server (isolated from the root workspace).
# Build from the repository root:
#   docker build -f examples/kv-tokio/bench/openraft/Dockerfile .

FROM rust:1.91-bookworm@sha256:c1e5f19e773b7878c3f7a805dd00a495e747acbdc76fb2337a4ebf0418896b33 AS rust
WORKDIR /src
COPY examples/kv-tokio/bench/openraft/Cargo.toml examples/kv-tokio/bench/openraft/Cargo.lock examples/kv-tokio/bench/openraft/build.rs ./
COPY examples/kv-tokio/bench/openraft/proto ./proto
COPY examples/kv-tokio/bench/openraft/src ./src
COPY examples/kv-tokio/proto ./vendor-proto
RUN cargo build --release --locked --bin openraft-kv

FROM debian:bookworm-20261005-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
COPY --from=rust /src/target/release/openraft-kv /usr/local/bin/openraft-kv
