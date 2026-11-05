# Local NativeLink service for WebRTC builds

Run one per-user service for all your worktrees on a trusted Linux x86_64
machine. In a terminal, start the pinned NativeLink 1.8.3 CAS, action cache,
scheduler and worker in the foreground:

```sh
./bazel run //tools/nativelink:server
```

Leave it running. In another terminal, from any worktree, use REAPI for the
WebRTC build:

```sh
./bazel test --config=webrtc-reapi //crates/pulsebeam-libwebrtc-sys:fast
```

The same config can be used with normal consumer pipeline commands. Stop the
service with Ctrl-C when it is no longer needed. Each worktree must keep its own
Bazel output base; share the service, not output bases. Normal Bazel builds use
offline Siso and do not require the service. For local execution with a shared
cache instead, use `--config=webrtc-cache`. For the explicit local fallback, use
`--//crates/pulsebeam-libwebrtc-sys:executor=local`. See the [WebRTC build
guide](../../crates/pulsebeam-libwebrtc-sys/README.md#build-ownership-and-caching)
for execution and caching boundaries.

## Configuration and recovery

The REAPI endpoint defaults to `127.0.0.1:50051` and the worker endpoint to
`127.0.0.1:50061`. State defaults to `~/.cache/pulsebeam/nativelink/v1`, with
mode `0700`. The server accepts `--state` (an absolute path), `--cpu` (positive,
default `min(8, cpu_count)`), `--memory-kb` (default `8388608`), `--reapi-port`
and `--worker-port`. For example:

```sh
./bazel run //tools/nativelink:server -- --cpu 4 --reapi-port 50052
./bazel test --config=webrtc-reapi \
  --//crates/pulsebeam-libwebrtc-sys:reapi=grpc://127.0.0.1:50052 \
  //crates/pulsebeam-libwebrtc-sys:fast
```

Run these commands in separate terminals. Custom REAPI endpoints must be
loopback. If a build cannot connect, check that the service is still running and
that the client and server ports match. If a port is occupied, stop the other
service or select distinct free ports. Restart the service after a failure; use
the explicit local fallback above if it is unavailable. REAPI uses strict remote
execution, so a failed remote compile does not silently run locally.

## Limits and trust

Both loopback endpoints are unauthenticated. Other local users able to connect
can submit work or poison the cache, so use this only on a trusted single-user
machine. The worker uses a pinned Nix bubblewrap entrypoint to deny network and
expose only the action work tree, Nix store and host loader libraries. This is a
local setup; a remote host fleet has not been verified.

Actions reserve 1 CPU and 1 GiB by default. CPU and memory capacities control
scheduler admission, not hard cgroup quotas. GN, Rust, and Siso's local-only
generators and archiving remain client-local, with two local Siso jobs per
client. The service does not govern every process on the machine. The CAS and
worker CAS each target 2 GB for eviction, and the action cache targets 200 MB;
these are not hard disk quotas. Leave disk headroom.

Cold duplicate work can join an in-flight action only when its cacheable action
key is identical. No multi-agent performance result is claimed. NativeLink is
licensed under FSL-1.1-Apache-2.0, which explicitly permits internal use.
