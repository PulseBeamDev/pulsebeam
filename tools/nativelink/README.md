# Local NativeLink service for Bazel builds

On a trusted single-user Linux x86_64 machine, use ordinary commands from the
repository root:

```sh
./bazel build //crates/pulsebeam-libwebrtc-sys:pulsebeam-webrtc-sys
./bazel test //crates/pulsebeam-libwebrtc-sys:fast
```

Ordinary `./bazel build`, `test` and `run` commands automatically start and
share a per-user local NativeLink service across worktrees. Eligible Bazel
actions use its remote cache and execution scheduler; the inner WebRTC/Siso
build also uses shared REAPI by default. Actions explicitly marked local or
no-remote remain local. No special config or separate service startup is
required. First use provisions the pinned NativeLink binary and host tools and
may require network access. Each worktree must keep its own Bazel output base;
share the service, not output bases. The launcher includes the ready worker's
pinned host-tools identity in Bazel action properties so runtime-tool changes
cannot reuse an incompatible cached result.

Pass `--config=local` to opt the whole build out of NativeLink remote caching
and execution. The narrower `--//crates/pulsebeam-libwebrtc-sys:executor=local`
only runs inner WebRTC/Siso offline; it does not disable remote caching or
execution for ordinary Bazel actions. For inner WebRTC local execution with a
shared cache instead, use `--config=webrtc-cache`. Siso REAPI uses strict remote
execution, so a failed remote compile does not silently run locally. See the
[WebRTC build guide](../../crates/pulsebeam-libwebrtc-sys/README.md#build-ownership-and-caching)
for execution and caching boundaries.

## Advanced: foreground service

As an alternative, run the pinned NativeLink 1.8.3 CAS, action cache, scheduler
and worker in the foreground:

```sh
./bazel run --config=local //tools/nativelink:server
```

The local config prevents the server-launching command from autostarting a
separate service. Leave the foreground service running while using it from
another terminal. For a manually started service at the default endpoint, set
`PULSEBEAM_NATIVELINK_AUTOSTART=0` on the client command so the launcher does
not try to manage it. Also set the worker host-tools property as shown below,
so manually configured clients retain the same cache-identity boundary. Stop
the foreground service with Ctrl-C when it is no longer needed.

## Configuration and recovery

The REAPI endpoint defaults to `127.0.0.1:50051` and the worker endpoint to
`127.0.0.1:50061`. State defaults to `~/.cache/pulsebeam/nativelink/v1`, with
mode `0700`. By default the scheduler advertises all CPUs in the service's
CPU affinity and the host's total memory. Bazel submits up to twice the CPU
capacity (minimum 8, maximum 128 jobs) to hide scheduling and CAS latency;
explicit client `--jobs` overrides this. The shared capacity is fixed at
service startup.

To verify in-flight deduplication and subsequent cache reuse with isolated Siso
clients and a temporary service, run the focused integration test:

```sh
./bazel test --config=local //tools/nativelink:reapi_integration_test
```

The test starts its own loopback service on temporary ports. It does not use the
shared service or its cache. To summarize a retained Siso INFO log, run
`python3 tools/nativelink/siso_summary.py /path/to/siso.INFO`. The summary
reports completed backend cache outcomes and queue times. Matching digests
alone do not prove that concurrent requests were joined.

The server accepts `--state` (an absolute path), `--cpu` (positive),
`--memory-kb` (positive), `--reapi-port` and `--worker-port` to configure
a foreground service. For example:

```sh
./bazel run --config=local //tools/nativelink:server -- --cpu 4 --reapi-port 50052
PULSEBEAM_NATIVELINK_AUTOSTART=0 ./bazel test --config=webrtc-reapi \
  --remote_executor=grpc://127.0.0.1:50052 \
  --remote_cache=grpc://127.0.0.1:50052 \
  --remote_default_exec_properties="pulsebeam-host-tools=$(<.bazel/output/external/+webrtc+webrtc_host/host-bin)" \
  --//crates/pulsebeam-libwebrtc-sys:reapi=grpc://127.0.0.1:50052 \
  //crates/pulsebeam-libwebrtc-sys:fast
```

Run these commands in separate terminals. The client sets both Bazel remote
endpoints and the inner WebRTC REAPI endpoint to the foreground service, and
disables autostart for that command. Custom REAPI endpoints must be loopback.
If a build cannot connect, check that the client and server ports match. If a
port is occupied, stop the other service or select distinct free ports. Use
`--config=local` if the service is unavailable.

## Limits and trust

Both loopback endpoints are unauthenticated. Other local users able to connect
can submit work or poison the cache, so use this only on a trusted single-user
machine. The worker uses a pinned Nix bubblewrap entrypoint to deny network and
expose only the action work tree, Nix store and host loader libraries. This is a
local setup; a remote host fleet has not been verified.

Bazel actions reserve 1 CPU and 2 GiB by default; Siso actions reserve 1 CPU
and 1 GiB. Simulator test actions reserve their four-child budget explicitly.
Scheduler reservations provide shared admission control, not hard OS limits.
Actions can use more resources than they reserve, and local processes are not
included in that budget. The outer WebRTC GN action and
Siso's local-only generators and archiving remain client-local, with two local
Siso jobs per client. Tests and generators may also stay local when their
contracts require it. The service does not govern every process on the machine.
The CAS targets 64 GiB for eviction, the worker CAS 8 GiB, and the action cache
1 GiB; these are not hard disk quotas. Leave disk headroom.

NativeLink is licensed under FSL-1.1-Apache-2.0, which explicitly permits
internal use.
