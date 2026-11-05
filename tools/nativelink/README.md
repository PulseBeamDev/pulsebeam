# NativeLink local execution candidate

Generate a standalone configuration for NativeLink **v1.8.3** with Python 3. Run only trusted builds on a trusted single-user machine: both loopback endpoints are unauthenticated, and actions execute with the worker user's privileges, without container or namespace isolation. Any local user able to connect can submit work or poison the cache.

```sh
umask 077
mkdir -p "$HOME/.cache/pulsebeam-nativelink"
python3 tools/nativelink/config.py \
  --state "$HOME/.cache/pulsebeam-nativelink" --cpu 4 --memory-kb 4194304 \
  > "$HOME/.cache/pulsebeam-nativelink/config.json"
nativelink "$HOME/.cache/pulsebeam-nativelink/config.json"
```

Run the pinned binary in the foreground as an unprivileged user. Stop it with Ctrl-C. REAPI listens on `127.0.0.1:50051`; the worker API listens on `127.0.0.1:50061`. Set `--reapi-port` and `--worker-port` to distinct available ports for another instance. Each instance needs its own private state directory. The state path must be absolute and cannot contain `$` because NativeLink expands environment variables in paths. Protect it against other local users and symlink manipulation.

`--cpu` and `--memory-kb` set the worker's scheduler capacity. When omitted by an action, `--action-cpu` and `--action-memory-kb` reserve 1 CPU and 1,048,576 KiB by default. Supplied action values are preserved; the default must fit the worker. Actions requesting more than capacity cannot run. Reservations are accounting, not CPU or memory enforcement. A client can request zero or too little, so this is not a resource security boundary. Tune capacities and defaults to the workload; no workload-specific Siso behavior has been validated here.

The CAS and worker fast CAS each have a 2,000,000,000-byte eviction target; the action cache has a 200,000,000-byte target. These are not disk quotas: temporary files, in-flight data, action work directories, filesystem overhead and delayed eviction can consume more. Arrange external disk headroom and cleanup. The priority-only `OSFamily` and `container-image` properties follow the upstream example and do not enforce platform compatibility or launch a container. Shared caches are unsafe across incompatible hosts, toolchains or trust domains unless the integration accounts for them. The config does not provide authentication, TLS, a hard process limit, network isolation, action timeouts, or lifecycle management.

## Checks

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/nativelink -p 'test_*.py' -v
NATIVELINK_BINARY=/absolute/path/to/nativelink PYTHONDONTWRITEBYTECODE=1 \
  python3 -m unittest discover -s tools/nativelink -p 'test_*.py' -v
```

The optional startup smoke test uses ephemeral loopback ports and a temporary state directory, then terminates its process group with a bounded timeout. Port allocation has a small bind race. It checks listeners and process startup, not action execution or real Siso interoperability.

## Upstream basis

The topology is adapted from [`local_rbe_self_test.json5` at v1.8.3](https://github.com/TraceMachina/nativelink/blob/v1.8.3/nativelink-config/examples/local_rbe_self_test.json5), commit `bd336137d794a6a4e7ac32e561ab55e2d7e70ff3`. Unlike the example, both listeners bind loopback, and memory is a scheduler resource. In that version, [`schedulers.rs`](https://github.com/TraceMachina/nativelink/blob/v1.8.3/nativelink-config/src/schedulers.rs) defines `property_modifier`, ordered `remove`/`replace`/`add` edits, and `minimum` resource properties. [`property_modifier_scheduler.rs`](https://github.com/TraceMachina/nativelink/blob/v1.8.3/nativelink-scheduler/src/property_modifier_scheduler.rs) implements those edits as map operations, with `add` overwriting an existing property. The modifier temporarily renames explicit values, adds defaults, then restores the explicit values; it clears its temporary names first. [`default_scheduler_factory.rs`](https://github.com/TraceMachina/nativelink/blob/v1.8.3/nativelink-scheduler/src/default_scheduler_factory.rs) passes the nested scheduler's worker interface through the modifier, and [`worker.rs`](https://github.com/TraceMachina/nativelink/blob/v1.8.3/nativelink-scheduler/src/worker.rs) accounts for minimum properties when allocating and releasing work. This is a candidate component, not a PulseBeam integration decision.
