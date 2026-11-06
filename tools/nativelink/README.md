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
pinned compiler host-tools and OS runtime identities in Bazel action properties,
so changes to either cannot reuse an incompatible cached result. The inner Siso
compiler actions use their separate compiler host-tools identity.

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
CPU affinity and 75% of the host's total memory, leaving RAM headroom for local
Bazel analysis, agents and the coordinator. Linux handles CPU fairness; there
is no separate CPU percentage cap. Bazel submits twice the CPU capacity to hide
scheduling and CAS latency; explicit client `--jobs` overrides this. The shared capacity is fixed at
service startup. The worker shares a pool of 64 input-blob downloads across
all actions, rather than multiplying download fan-out with each client's
cold build. This does not limit cached-input hardlinking or action processes.

Worker heartbeats retain their 2.5-second cadence. The scheduler allows
120 seconds without a heartbeat before treating the worker as stale, instead
of NativeLink's 10-second default. This tolerates loaded-host pauses without
immediately reassigning still-running operations; an unresponsive worker can
therefore take up to two minutes to detect. The scheduler's fallback grace
after an action deadline also becomes 120 seconds; the worker's own input,
command and upload timeouts remain enforced independently. This is not a
guarantee against arbitrary host stalls or resource exhaustion.

To verify in-flight deduplication and subsequent cache reuse with isolated Siso
clients and a temporary service, run the focused integration test:

```sh
./bazel test --config=local //tools/nativelink:reapi_integration_test
```

The test also checks the worker's CA roots, localhost resolution, dual-stack
loopback traffic and installer tools inside the remote action. It starts its
own loopback service on temporary ports and does not use the shared service or
its cache. Each worktree still analyzes its own Bazel graph,
hashes inputs and prepares its own outputs. Matching action names in client
progress do not prove duplicate worker execution: NativeLink can join
identical in-flight action digests, while different inputs or flags require
separate actions.

After service-defining build-system changes, stop active builds, update the
agent worktrees to the same build-system revision, and restart the shared
service. Autostart rejects incompatible live services rather than silently
using stale worker settings or killing another build's backend.

To summarize a retained Siso INFO log, run
`python3 tools/nativelink/siso_summary.py /path/to/siso.INFO`. The summary
reports completed backend cache outcomes and queue times. Matching digests
alone do not prove that concurrent requests were joined.

The server accepts `--state` (an absolute path), `--cpu` (positive),
`--memory-kb` (positive), `--cache-bytes` (positive, per CAS tier), `--reapi-port`
and `--worker-port` to configure a foreground service. Explicit CPU, memory and
cache values override automatic sizing. For example:

```sh
./bazel run --config=local //tools/nativelink:server -- --cpu 4 --reapi-port 50052
PULSEBEAM_NATIVELINK_AUTOSTART=0 ./bazel test --config=webrtc-reapi \
  --remote_executor=grpc://127.0.0.1:50052 \
  --remote_cache=grpc://127.0.0.1:50052 \
  --remote_default_exec_properties="pulsebeam-host-tools=$(<.bazel/output/external/+webrtc+webrtc_host/host-bin)" \
  --remote_default_exec_properties="pulsebeam-runtime=$(<.bazel/output/external/+os_inputs+host_runtime/runtime-root)" \
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
machine. The pinned Nix bubblewrap entrypoint isolates each action's network
to IPv4/IPv6 loopback. It exposes the action work tree, Nix store, host loader
libraries and a separately pinned OS runtime. That runtime supplies `curl`,
`file`, `tar`, `xz`, CA roots and deterministic `localhost` entries without mounting the
host's `/etc` or home directories. `SSL_CERT_FILE` defaults to the runtime's CA
bundle; an explicit action override is preserved. Updating this runtime does
not change the WebRTC compiler host-tools identity. This is a local setup; a
remote host fleet has not been verified.

Bazel actions reserve 1 CPU and 2 GiB by default; Siso actions reserve 1 CPU
and 1 GiB. Simulator test actions reserve their four-child budget explicitly.
Scheduler reservations provide shared admission control, not hard OS limits.
Actions can use more resources than they reserve, and local processes are not
included in that budget. The outer WebRTC GN action and
Siso's local-only generators and archiving remain client-local, with two local
Siso jobs per client. Tests and generators may also stay local when their
contracts require it. The service does not govern every process on the machine.
At startup, each CAS tier targets 10% of the capacity of the filesystem
containing the state directory; the action cache targets 1% of one CAS tier.
Matching the two CAS targets avoids a smaller worker tier repeatedly evicting
and refetching inputs while staging large concurrent builds. These targets do
not preallocate space and are not hard disk quotas; executable copies and active
action files can consume additional space. Leave disk headroom and use
`--cache-bytes` to reduce the targets on a nearly full filesystem.

NativeLink is licensed under FSL-1.1-Apache-2.0, which explicitly permits
internal use.
