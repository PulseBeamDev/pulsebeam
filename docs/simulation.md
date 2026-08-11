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

## Four kinds of oracle

An oracle is whatever decides that a run was wrong. This suite has four, and they fail in
different ways, so a plan is stronger for using more than one.

**User-visible outcome** — "the message arrived", "the tile is not black". Self-justifying, and
every unambiguous defect found so far has been one of these. Limited to failures someone thought
to check.

**Threshold** — "the estimate reaches 80% of need". Necessary for congestion control and the
easiest to get wrong: `QueueingDelayBelow` bounded a peak while documenting a standing queue, and
`a_busy_room_starves_nobody` needed an estimator to hit 86% before an allocator claim could pass.
Every threshold needs an argument that a correct implementation can meet it.

**Differential** — two configurations that must agree, where the disagreement is the report.
`sharding_does_not_change_who_is_served` runs a scenario on one shard and on three: a subscriber
cannot tell which worker owns it, so any difference is the SFU losing a stream to its own
placement. No number to pick and no way to measure the wrong statistic. Both cross-shard defects
found so far are differences it reports directly. Compare coarsely — whether a subscriber was
served, not how many bytes — because two layouts schedule packets differently and byte counts
would report noise.

**Liveness** — every other oracle reads an endpoint or an average, and a stream that freezes for
twenty seconds and recovers looks identical in all of them. `a_started_stream_does_not_go_quiet`
bounds the longest gap between delivered frames. Measure per frame, not per plan step: a step is
tens of seconds long, so a freeze inside one still leaves bytes in the window and reads as zero
silence. The first attempt did exactly that and was vacuous — it passed with the bound set to
zero.

## Measuring anything new

Four QoE metrics were added in one sitting and all four were wrong before they were right. The
failures were not subtle bugs; each produced a plausible-looking number that meant nothing. A
metric that lies is worse than no metric, because it gets trusted.

**Prove the measurement moves before trusting it.** Set the bound to zero and confirm the test
fails. The first liveness check sampled at `Run` boundaries, where a freeze inside a
thirty-second window still leaves bytes in the window — it passed with the bound at
`Duration::ZERO` and would have shipped as a green test measuring nothing.

**Never compare timestamps taken on different turmoil hosts.** turmoil virtualises
`tokio::time::Instant` per host, so a stamp from the coordinator and one from inside a participant
are on different epochs. That mismatch reported every time-to-first-frame as ~5s and every freeze
as 0% of the session. Use `std::time::Instant`, which `sim_clock` shims process-wide via
`clock_gettime` and which therefore *is* coherent everywhere.

**Match the scope of the numerator and denominator.** Cumulative frame counts over the last
window's duration reported 159 fps; a cumulative maximum freeze next to a per-window frozen total
reported "5s freeze, 0% frozen". Pick session-scoped or window-scoped and apply it to both.

**Measure from what the user did, not from what the plan did.** Time-to-first-frame anchored to
participant creation captured each plan's five-second "establish connection" step, putting the
median at 5.18s across the suite — an artefact of scaffolding presented as a product latency.

**Check a new bar against the whole suite before believing it.** The first QoE bar called 491 of
492 viewers broken. By the triage rule above that is an unrealistic expectation, not a codebase in
ruins — and it was: freeze limits were being applied to still screen shares, which are supposed to
go quiet.

## Injecting failures

Recovery code is the least tested code in any system, because it only runs when something goes
wrong and nothing goes wrong in a simulator unless the simulator makes it. Around ninety
`debug_assert!`/`fatal!` sites here assert a condition cannot arise, and the route-install callers
each have a rollback that had never executed.

`buggify!("site")` declares such a point; `.with_buggify(permille)` arms it for a plan. Off
everywhere else, so the rest of the suite keeps testing the happy path, and compiled out entirely
without the `sim` feature.

Two rules learned immediately:

- **Assert that something was injected.** At 80 per thousand the first chaos plan injected nothing
  on the first seed and passed — indistinguishable from a real pass.
- **Watch which sites are reached.** `coverage()` reports reached and fired. A site nothing reaches
  is a failure path still untested and looks exactly like a covered one, so
  `every_declared_failure_point_is_reachable_test` fails if none is reached or none fires.

It found a defect on its first real run: a failed reverse-route install publishes the track anyway
with no reverse handle, so keyframe requests for it are dropped for the life of the track.

## Assert over every instance

A plan that creates N of something and checks one is a coin flip dressed as an assertion. The
cross-shard data-topic defect survived precisely that: two subscribers, one asserted. Where a
participant is a deliberate bystander, assert it received *nothing* — that is a real claim about
not over-delivering, and it was untested.

## What the suite still cannot see

Worth writing down, because an absent capability looks exactly like a passing test.

**Whether the client was told anything.** `RemoteTrack` exposes `publisher_id()` and `recv()`.
There is no track state and no pause notification, so a subscriber cannot distinguish "the SFU
paused this stream" from "the network died" - both are simply an absence of packets. A viewer
therefore sees a blank tile where a placeholder would do, and no assertion can be written about it
until the product exposes the state. The simulation can see that a stream was paused
(`forwarded_quality == 0`); the client cannot.

**Audio.** Every QoE figure here is video. Audio has no continuity, freeze or decodability
measurement at all, and audio breaking up is at least as noticeable as video freezing.

**Temporal layers.** Generated scenarios never publish with `with_temporal_dd`, so shedding
framerate instead of pausing a stream - the graceful degradation a weak link should get - is
untested by any property.

**Torn frames.** 1,370 across a suite run, measured and reported, asserted on by nothing. A frame
preceded by a sequence hole is visible corruption.

**Time-to-first-frame and freezes** are measured and on the scoreboard, but not gated, because the
product currently fails both. See the note in `properties.rs`.

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
