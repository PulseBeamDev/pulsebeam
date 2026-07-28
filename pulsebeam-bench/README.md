# pulsebeam-bench

A `criterion` benchmark that runs a real `pulsebeam::node::NodeBuilder` node
and real `pulsebeam-agent` clients over loopback in one process, and reports
**shard-thread CPU per MiB of media delivered to clients**.

Why CPU instead of wall time, and why per byte instead of per iteration:
the load generator shares the machine with the SFU, so wall time partly
measures the generator's mood on the day. CPU time charged to the SFU's own
`pb-w-*` threads (read from `/proc/self/task/<tid>/schedstat`, so it costs
nothing during preemption) does not have that problem. And because the test
media is constant-bitrate, "bytes delivered" is a stable unit of work — a
faster SFU delivers the same bytes for less CPU, not more bytes for the same
CPU, so the two framings agree.

## Running it

```sh
make bench                                          # compare to the last saved baseline
make bench BENCH_ARGS="-- --save-baseline before"    # snapshot the current tree
git stash                                            # or: git checkout <ref>
make bench BENCH_ARGS="-- --baseline before"         # compare against it, with a p-value
git stash pop
```

Or directly:

```sh
cargo bench -p pulsebeam-bench                       # ad hoc run
cargo bench -p pulsebeam-bench -- --save-baseline X   # save
cargo bench -p pulsebeam-bench -- --baseline X        # compare
```

Reports land in `target/criterion/forwarding/<scenario>/report/index.html`.
criterion resamples across runs and reports a confidence interval and
regression/improvement verdict — that statistical framing is the point of
using criterion here rather than a one-off timing script: "X% faster" is a
claim about a distribution, not one lucky run.

## Scenarios

Fixed in `benches/forwarding.rs`, not CLI-configurable: criterion keys its
history off the benchmark id, so a scenario whose shape can change from the
command line would silently invalidate its own baseline.

| id | shards | rooms | peers/room | exercises |
|---|---|---|---|---|
| `1shard/1room/4peers` | 1 | 1 | 4 | per-packet cost, narrowest signal |
| `1shard/4rooms/4peers` | 1 | 4 | 4 | routing/demux over more streams, tick coalescing |
| `2shards/4rooms/4peers` | 2 | 4 | 4 | cross-shard fanout |

Each simulated peer publishes 1 video + 1 audio track and subscribes to 7
video + 3 audio tracks, matching `pulsebeam-cli bench`'s shape.

Adding a scenario: add a `Scenario::new(...)` to `SCENARIOS` in
`benches/forwarding.rs`. Size it first with `make bench-probe` (see below) —
a scenario run at >85% shard utilization measures queueing, not efficiency,
and the benchmark asserts this and panics rather than publish that number.

## Sizing a new scenario

```sh
make bench-probe SHARDS=1 ROOMS=8 PEERS=4
```

Prints per-sample CPU/MiB, throughput, and shard utilization so you can pick
a room/peer count that sits comfortably under the utilization ceiling before
committing it to `benches/forwarding.rs`.

## Requirements

- Linux with `/proc/*/task/*/schedstat` (`CONFIG_SCHEDSTATS`, on by default on
  virtually every distro kernel). The harness checks this up front and fails
  with an explanation rather than silently reporting zero.
- Loopback networking only — no external ports, no root.

## Notes on the implementation

- The node's shard threads are **not joined on drop**. `ShardWorker::run`
  only returns after joining its own spawned threads while still holding the
  channels that let them exit — cancelling the token does not unblock that
  join. So a benchmark run leaves the previous scenario's shard threads
  parked idle in epoll (no timers armed, no work, no CPU) rather than
  hanging. `DataPlaneClock` tracks each scenario's threads by tid, taken
  before and after that scenario's node starts, so idle leftover threads
  from an earlier scenario are never counted.
- Every shard binds UDP with `SO_REUSEPORT`, so the harness reserves one
  concrete loopback port up front instead of letting each shard's socket
  pick its own ephemeral port.
