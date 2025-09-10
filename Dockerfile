FROM docker.io/clux/muslrust:1.89.0-stable as builder

COPY Cargo.* .
COPY . .
RUN --mount=type=cache,target=/volume/target \
    --mount=type=cache,target=/root/.cargo/registry \
    cargo build --release -p pulsebeam && \
    mv /volume/target/x86_64-unknown-linux-musl/release/pulsebeam /volume/pulsebeam-bin


FROM cgr.dev/chainguard/static
# FROM alpine:3.22

WORKDIR /app
COPY --from=builder --chown=nonroot:nonroot /volume/pulsebeam-bin /app/pulsebeam

EXPOSE 3478/udp 3000

ENTRYPOINT ["/app/pulsebeam"]

