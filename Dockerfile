FROM docker.io/library/rust:1.92.0 AS builder
RUN apt update && apt install -y protobuf-compiler libprotobuf-dev
WORKDIR /app

COPY . .

RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    make release && \
    cp /app/target/release/pulsebeam /app/pulsebeam-bin


FROM gcr.io/distroless/cc-debian13:nonroot
WORKDIR /app
COPY --from=builder --chown=nonroot:nonroot /app/pulsebeam-bin /app/pulsebeam
EXPOSE 3478/udp 3000
ENTRYPOINT ["/app/pulsebeam"]
