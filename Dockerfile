FROM docker.io/library/rust:1.84-alpine AS builder

RUN apk add make protobuf-dev musl-dev
WORKDIR /app

# https://stackoverflow.com/a/64141061
# https://docs.docker.com/build/cache/optimize/#use-cache-mounts
COPY . .
RUN --mount=type=cache,target=/app/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    cargo build --release && \
    # Copy executable out of the cache so it is available in the final image.
    cp target/release/pulsebeam-server-lite ./pulsebeam-server-lite

FROM docker.io/library/alpine:3

WORKDIR /app
COPY --from=builder /app/target/release/pulsebeam-server-lite .

ENTRYPOINT ["./server-rs"]

