# Simulation testing

The SFU is tested by running it against a simulated network — simulated clock,
simulated randomness, modelled capacity, queueing, loss, reordering and
partitions. The approach is FoundationDB's and TigerBeetle's: **one seed
determines one entire run**, so anything found can be replayed exactly.

## Three commands

| Command | What it is |
|---|---|
| `make test` | what CI runs: 584 unit tests + 71 simulation plans |
| `make test-sim-seed SEED=<n>` | replay one seed exactly |
| `make sim-sweep` | search for seeds nobody has tried |

## Two rules

**Gates are deterministic and committed.** Every seed CI runs is a constant in
source (`DEFAULT_SIM_SEED`, or a plan's own `QOS_SEEDS`). CI never draws a random
seed, so a CI failure is always real and always reproducible. It cannot flake.

**Search is advisory and never blocks.** `sim-sweep` explores new seeds — nightly
in CI (`.github/workflows/nightly.yml`, over a window that advances each run) and
on demand locally. Its only product is a seed to promote. It is not a gate, and a
sweep failure is not a broken build.

## The loop

```
   sweep ──> "seed 4711 fails tests::bwe::foo"
               │
               ▼
     make test-sim-seed SEED=4711        reproduce
               │
               ▼
          fix the bug
               │
               ▼
     add 4711 to that plan's seed list   promote
               │
               ▼
   every PR ──> make test                guarded forever
```

Promotion is the point. A seed that found a bug once becomes a committed
constant, so the bug can never come back silently. Search finds; the gate keeps.

## Promoting a seed

Plans that run under several seeds loop over a constant:

```rust
// pulsebeam-simulator/src/tests/bwe.rs
const QOS_SEEDS: [u64; 4] = [0xDEAD_BEEF, 0x1234_5678, 0x0BAD_F00D, 0xFEED_FACE];

#[test]
fn some_plan_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new().with_rng_seed(seed) /* ... */
    }
}
```

Add the discovered seed to that array — or to the individual plan, if it is only
meaningful there. A plan that does not yet loop over seeds should start.

## What the seed controls

One seed reaches all of it. If any of these stopped being seeded, a failure
would stop being reproducible, so two meta-tests guard it:

| Source | Seeded via |
|---|---|
| Wall clock | `sim_clock.rs`, shims `clock_gettime` process-wide |
| `getrandom` (keys, map ordering) | `sim_rand.rs` |
| Network latency jitter, delivery | turmoil's `rng_seed` |
| Loss, reordering, duplication | `shaper::seed_impairments` |
| Generated scenarios | proptest runner seeded in `properties.rs::check` |

- `properties.rs::the_seed_selects_the_generated_scenarios` — the seed reaches
  the generator.
- `bwe.rs::a_different_seed_is_a_different_network_test` — the seed reaches the
  network.

Both are cheap and both fail loudly, because a suite that *looks* randomised but
replays one fixed run is worse than an honestly fixed one.

## Two kinds of plan

**Authored plans** (`bwe.rs`, `video.rs`, `connectivity.rs`, `data_channel.rs`)
pin a scenario someone thought of. Good for regressions, limited to the failures
we already imagined.

**Generated plans** (`properties.rs`) generate the scenario and assert a claim
that must hold whatever comes out. Properties are deliberately weak — they are
claimed over every network the generator can produce, so they can only assert
what is true of all of them. Failures persist to `proptest-regressions/`, which
is committed and replayed before any new case.

## The behaviour scoreboard

`bwe-baseline.txt` is a committed, diffable record of what every authored plan
measured — capacity, estimate, drawdown, demand, queueing, loss. Regenerate with
`make bwe-baseline` and read the diff.

It is not a test. Its job is that a congestion-control change rarely improves
everything: it trades one scenario against another, and pass/fail hides that.
The diff shows the whole matrix at once, so "helps screenshare, wrecks cellular"
is one line rather than a discovery made days later.

Generated plans are excluded from it, because their numbers move by design.
