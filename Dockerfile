# syntax=docker/dockerfile:1.7
FROM rust:1-bookworm AS builder
WORKDIR /app

# Dependency layer: build against stub sources so `cargo build` of the real
# code only recompiles this crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked && rm -rf src

COPY src src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
ENV DATA_DIR=/data LISTEN_ADDR=0.0.0.0:9200
EXPOSE 9200
USER nonroot:nonroot
COPY --from=builder /app/target/release/docstore /usr/local/bin/docstore
ENTRYPOINT ["/usr/local/bin/docstore"]
