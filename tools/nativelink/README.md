# Local NativeLink service for WebRTC builds

On a trusted single-user Linux x86_64 machine, use ordinary commands from the
repository root:

```sh
./bazel build //crates/pulsebeam-libwebrtc-sys:pulsebeam-webrtc-sys
./bazel test //crates/pulsebeam-libwebrtc-sys:fast
```

Ordinary `./bazel build`, `test` and `run` commands automatically start and
share the local NativeLink service across worktrees. Shared REAPI execution is
the default for WebRTC builds; no special config or separate service startup is
required. First use provisions the pinned NativeLink binary and host tools and
may require network access. Each worktree must keep its own Bazel output base;
share the service, not output bases.

For the explicit local fallback, which runs Siso offline, pass
`--//crates/pulsebeam-libwebrtc-sys:executor=local` to the Bazel command. For
local execution with a shared cache instead, use `--config=webrtc-cache`. REAPI
uses strict remote execution, so a failed remote compile does not silently run
locally. See the [WebRTC build guide](../../crates/pulsebeam-libwebrtc-sys/README.md#build-ownership-and-caching)
for execution and caching boundaries.

## Advanced: foreground service

As an alternative, run the pinned NativeLink 1.8.3 CAS, action cache, scheduler
and worker in the foreground:

```sh
./bazel run //tools/nativelink:server
```

Leave it running while using it from another terminal. For a manually started
service at the default endpoint, set `PULSEBEAM_NATIVELINK_AUTOSTART=0` on the
client command so the launcher does not try to manage it. Stop the foreground
service with Ctrl-C when it is no longer needed.

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
loopback. If a build cannot connect, check that the client and server ports
match. If a port is occupied, stop the other service or select distinct free
ports. Use the explicit local fallback above if the service is unavailable.

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

NativeLink is licensed under FSL-1.1-Apache-2.0, which explicitly permits
internal use.
