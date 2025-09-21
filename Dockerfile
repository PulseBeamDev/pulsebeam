FROM docker.io/library/rust:1.90.0-alpine3.22 as builder
RUN apk add --no-cache musl-dev perl make protobuf-dev
WORKDIR /app

COPY . .

RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    cargo build --release -p pulsebeam && \
    cp /app/target/release/pulsebeam /app/pulsebeam-bin

FROM docker.io/chainguard/static
WORKDIR /app
COPY --from=builder --chown=nonroot:nonroot /app/pulsebeam-bin /app/pulsebeam
EXPOSE 3478/udp 3000
ENTRYPOINT ["/app/pulsebeam"]
