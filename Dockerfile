FROM docker.io/library/rust:1.89.0-alpine3.22 as builder

RUN apk add --no-cache musl-dev perl make protobuf-dev
COPY . .
WORKDIR /app
RUN --mount=type=cache,target=/volume/target \
    --mount=type=cache,target=/root/.cargo/registry \
    cargo build --release -p pulsebeam && \
    mv target/release/pulsebeam pulsebeam-bin


# FROM docker.io/chainguard/static
FROM alpine:3.22

WORKDIR /app
COPY --from=builder --chown=nonroot:nonroot /app/pulsebeam-bin /app/pulsebeam

EXPOSE 3478/udp 3000

ENTRYPOINT ["/app/pulsebeam"]

