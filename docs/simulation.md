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

Regenerate it **before and after** any change to allocation or congestion
control, and read the diff before committing. That is not ceremony: the first
attempt at the starvation-deadlock fix passed its own target plan and pinned an
estimate 25% above a link's real capacity elsewhere, which the scoreboard caught
and no single test would have.

`>` truncates on start, so `make bwe-baseline` destroys the file if the run is
interrupted. Capture to a scratch file when experimenting.

## What a red test means

A failing simulation plan is one of four things, and they need opposite
responses. Deciding which comes first, before touching code.

| Signature | Reading |
|---|---|
| Wide margin, one seed | a real defect |
| Narrow margin, several seeds | the threshold is wrong |
| Fails at every seed | the scenario or the expectation is unrealistic |
| A `debug_assert!` in production code | an invariant someone wrote down is false |

Worked examples, all from one sweep:

- **33% of a required 80%**, one seed → real: an estimate frozen at 997 kbps
  with the link idle at 4.9%.
- **102ms and 108ms against a 100ms bound**, two seeds → the property was
  measuring the wrong statistic. It bounded the *peak* queue while documenting
  itself as the *standing* queue; the standing queue was 7ms and never moved.
- **1.6% short**, one seed → the plan was over-fitted. It needed the estimator
  to reach 86% of capacity before the allocator could pass, so an allocator
  claim rode on an estimator one.

Two corollaries. Never "fix" a threshold by relaxing it until it passes — decide
what it should measure. And never conclude "flaky": every plan is deterministic
in its seed, so a seed-dependent failure is a real difference in behaviour.

## Writing a plan or property

**Assert what a user would notice.** "The message arrived", "the tile is not
black", "the stream holds a layer". These are self-justifying. A claim in
internal quantities — "the estimate is ≥80% of need" — needs a separate argument
that a correct implementation can even meet it, and that argument is usually
missing. In this suite every unambiguous find has been user-visible; every
argument about whether a failure is real has been about an internal quantity.

**Keep one claim per plan.** A plan that needs the estimator to hit 86% before
the allocator can be judged is testing two things and will fail for the wrong
one.

**Generate coarsely.** An axis earns a value only if it crosses a decision
boundary in the code. Two capacities that afford the same ladder rungs are the
same test. `properties.rs` holds its own space to a band with a test, because
both directions fail silently: too large and a run samples a fraction of a
percent, too small and every seed runs the same cases.

**Put input assumptions in the generator, outcome assumptions in the body.**
`prop_assume!` on an input runs a full simulation and throws it away — one
property discarded two thirds of its cases that way and took twice as long as
any other.

**Prefer boundaries to ranges.** Threshold bugs live at rung edges, so generate
the rungs and the points just either side rather than sampling a wide range and
hoping.

## Fixing production code the sweep finds

**Reproduce first, and confirm the fix by removing it.** A test that has never
been seen failing pins nothing. Revert the fix, watch the test fail, restore it.
This has caught a "passing" probe that was silently a no-op.

**Test the decision, not the run.** Congestion-control predicates are pure
functions of `(desired, allocated, estimate, now)`. Extract them and unit-test
the shapes that must and must not trigger — milliseconds per iteration instead of
a five-minute suite. Iterating on a behaviour change by re-running the whole
simulation is how the first deadlock fix shipped a regression into a scoreboard
run instead of being caught in a unit test.

**Set thresholds from measurement.** The deadlock predicate fires below 25% of
the estimate because the real deadlock sits at 15% and a healthy allocator
backgrounding a stream sits at 44%. Write the measured figures into the comment
so the next person can requalify the number instead of guessing at it.

## Working through findings concurrently

Simulation runs are minutes long and mostly idle CPU-wise from the agent's point
of view, so the slow way to work is one plan at a time, waiting on each.

- **Batch the verification chain.** `fmt`, `lint`, `make test`, the scoreboard
  and the sweep run unattended as one job. Check back once.
- **Do not interleave source edits with a running simulation.** Cargo rebuilds
  mid-run and the results become a mix of two trees. Draft into a scratch file
  and apply when the run lands.
- **Investigate while it runs.** Reading code, extracting a predicate, and
  writing unit tests cost nothing and need no build.
- **Prefer the cheap signal.** A generator-level meta-test runs in 11ms and a
  predicate unit test in microseconds; both can falsify an idea before a suite
  run confirms it.
- **Keep one clean measurement at the end.** Concurrent runs that raced a build
  are worth discarding — take a single uncontended scoreboard and sweep on the
  final tree.
