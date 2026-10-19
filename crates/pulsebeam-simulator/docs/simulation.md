# Simulation testing

Use the [simulation contract](sim.md) for shared profiles, convergence invariants
and fault semantics. Use the [contributor testing contract](../../../docs/build.md#testing-contract)
for proof selection, gates and evidence reuse. [Owner commands](../README.md#commands)
cover isolated replay, advisory seed search and scoreboard generation.

## Choose the observation boundary

Keep pure allocation, admission and generator decisions in unit tests. Use an
authored simulation for a specific lifecycle, transport, decode or cross-shard
regression, and generated properties for claims that hold across their declared
network and demand premises. Remove duplicate proof only when the regression,
premises and observation boundary are the same.

A differential should compare user-visible behavior at the intended granularity.
For shard placement, compare decoded origins and whether each viewer was served,
not byte equality: different layouts can legitimately schedule different packets.
Frame counts do not replace decoder correctness, and a nonblank temporal-layer
experience does not establish watchable framerate or bounded freezes.

## Preserve deterministic isolation

Each selected case runs in its own process with one libtest thread. Clock and
randomness hooks are process-wide; do not aggregate cases by directly invoking
the raw test binary. A subprocess fixture is marked ignored and executed by its
parent with the required inputs, not counted as independent passing coverage.

One simulation seed controls clock hooks, process randomness, network impairment
and the proptest generator. Replay the exact case and seed printed on failure.
A seed-dependent failure is a reproducible behavior difference, not flakiness.
Advisory sweeps discover inputs to retain; they are not additional merge gates.

For measurements spanning turmoil hosts, use the shared simulated
`std::time::Instant` clock, not independently based host-local Tokio instants.
Match numerator and denominator scope: cumulative frame counts divided by a
single window do not measure that window's framerate. Anchor user latency to
the user's operation rather than an earlier connection-setup step.

## Share reports without losing claims

The ample-link family checks delivery, demand tracking and underused-link loss
on one report. The peer-churn family shares decode, layer-stability and estimate
checks. Shared scenarios correlate those claims; they are not independent
samples of each claim's original generator.

Preserve each claim's configured qualifying-case budget. Check every applicable
assertion before rejecting a report for a sibling's premise, then require the
conjunction of conditional premises for an accepted shared-family sample. This
keeps a rejected sibling from hiding a failure and ensures each conditional
claim receives its full accepted-case budget. Bound outcome-based rejections so
an unreachable premise fails rather than searching indefinitely. A deliberately
conditional assertion that does not reject must separately prove it was reached.

Generate input premises before simulation. Keep axes at named decision
boundaries and retain distinct fault, transport, decode, contention and
placement behavior rather than adding continuous values that exercise the same
decision repeatedly.

## Compare intent with coherent observations

`ControlStateOracle` models participants, publications and complete receive/topic
intent from plan operations. Admission observations bind opaque IDs to plan
identities; server mappings and forwarded packets are not the source of
expected intent.

Control, each shard and the native client actor produce immutable values through
their owned command boundaries. Compare their connection generations, revisions,
assignments and route epochs after bounded protocol progress. This is not a
global atomic snapshot, and separately sampled counters cannot reconstruct one.

Automatic audio must cover every live source when it fits, or fill the remaining
unpinned slots with live sources under contention. Packet timing, power decay
and slot immunity can change the contended subset, so the common oracle does not
invent a unique ordering. Explicit speaker-selection assertions retain the
scenario's loudness and origin claims.

A publication-discovery barrier must also wait for applicable full-client
convergence: receiving a catalog entry does not prove that publish-only peers
have updated catalogs or that every latest wire intent has been acknowledged.
Keep that work inside the existing bounded deadline, not a fixed extra sleep.

Graceful departure and transport crashes have different retirement deadlines.
Before crash detection, a server may legitimately retain the connection; after
the bounded retirement condition, no old publication, binding, route or topic is
exempt. A stopped observer need not update its final client snapshot. Check
positive media/data origin evidence for every intended recipient and absence
for every intentional bystander.

## Retain and replay generated failures

`src/tests/properties.regressions` is the declared committed corpus. Its entries
are proptest RNG states evaluated under each current strategy family, not
serialized historical scenarios. Historical shrink comments describe the former
generator and are not current scenario fixtures.

The persistence adapter copies that corpus into a writable per-family
`.regressions` artifact. Bazel test actions use `TEST_UNDECLARED_OUTPUTS_DIR`;
standalone replay uses a process-specific temporary directory. The printed
`[simulation-regressions]` line identifies the artifact. New minimized failures
are appended there, never written into tracked source by a normal test action.

To replay a saved artifact, provide its absolute path while retaining the case
and whole-run seed from the failure:

```sh
PULSEBEAM_SIM_REGRESSIONS=/absolute/path/to/family.regressions \
  ./bazel run //:replay -- --seed <seed> --exact --filter <case>
```

Review retained RNG states and promote useful regressions into the committed
corpus or a focused authored case. The isolated persistence proof exercises both
committed input under a current strategy and a newly minimized artifact in a
fresh process with no novel cases.

## Exhaust bounded topic executions

Run the required finite topic model independently of seeded sweeps:

```sh
./bazel run //:replay -- --exact --filter tests::exploration::bounded_cross_shard
```

This case belongs to the simulator's required `fast` owner. It re-executes exact
decision prefixes in fresh isolated processes until the declared tree is fully
traversed, then strictly replays a materialized leaf in another fresh process.
There is no state-hash pruning or opaque runtime snapshotting.

The model uses real native agents and SFU control/shard logic: two shards, one
publisher, two intended receivers and one unsubscribed bystander. A observes all
four connected with no topic declarations. After the declaration batch reaches
readiness, both receivers must observe two distinct origin-tagged 4,096-byte
ordered payloads. The publisher then disconnects and rejoins; after readiness,
both receivers must observe exactly the two old and two new payloads in order.
The publisher and bystander must observe none. Independent desired-state and
coherent owner oracles check retirement, failed-materialization atomicity,
incarnation fencing, routes and positive cross-shard delivery.

| Choice boundary | Finite alternatives |
| --- | --- |
| Declaration submissions, without settling between them | All six orders of one publication and two subscriptions |
| First host quantum after declarations and after rejoin | Canonical or reverse running-host order, with pending native protocol work |
| First eligible co-deliverable publisher UDP pair | Forward or reverse delivery |
| First eligible publisher application datagram | Nominal or an additional 3,100 ms delay |
| One named declaration/rejoin fault | None, one UDP loss, one UDP duplicate, 100 ms controller stall, wrong-owner steering, or failed materialization followed by an explicit fresh admission attempt |

Eligibility selects live DTLS application traffic, not keepalives. The delayed
original must remain queued while the prefix is delivered by retransmission,
then be released before completion. The delay crosses the pinned SCTP DATA
initial T3-RTX boundary of 3,000 ms. The quantum is 1 ms and A-to-B execution has
a single 60-second virtual deadline. Only one loss, duplicate or internal failure
is admitted per path; host order and finite latency are not additional faults.
No permanent outage, additional authored churn, arbitrary byte corruption, TCP
byte reordering or internal Tokio task-schedule enumeration is claimed.

Outside these opportunities the policy is canonical host/network order on the
seeded clean fiber profile. Protocol randomness stays fixed by the recorded seed
and test identity. Native topic declarations can reconfigure their transport as
part of normal agent behavior. Pre-A setup schedules are outside the theorem.
The simulator-only bounded case selects real RustCrypto DTLS operations with a
committed, deliberately insecure test certificate/key and seeded entropy. This
avoids AWS-LC hardware entropy bypassing the simulator's libc shim; it is not a
cryptographic security test. Production crypto and no-hook Turmoil scheduling
retain their default behavior.

The printed report records materialized inputs and bounds, completed branches,
choice coverage, disabled opportunities, consumed faults, pruning and measured
host cost. Every required alternative must actually be exercised. No branch is
pruned in this model. `counterexample`, `incomplete`, `invalid-model`,
`replay-divergence` and `infrastructure-failure` are non-success outcomes, not
skips or automatically passing retries. The default operational cutoffs are
4,096 paths and 600 host seconds. `--bounded-max-paths` and
`--bounded-max-seconds` change those cutoffs, not the model bounds; exhaustion
before completion reports `incomplete`.

Replay the retained artifact with the same binary identity:

```sh
./bazel run //:replay -- --bounded-replay /absolute/path/to/branch-0000.json
```

Artifacts contain model/build/dependency/fixture identity, materialized payloads
and seeds, ordered decision sites and enabled sets, selections, consumed faults,
virtual progress and property observations. Changed identity, inputs, enabled
sets, missing/extra/out-of-order decisions, unfired faults or changed semantic
observations are rejected. Cross-version artifact compatibility is not promised.
Seeded case replay, advisory seed sweeps and proptest regression files keep
their existing meanings. A seed sweep selects seeded cases only, not repeated
samples of this fixed bounded model.

## Keep recovery and fidelity claims honest

Recovery plans must force each required fault site, prove that it fired and
check post-fault atomicity and convergence. Branch counters alone establish
neither recovery nor continued delivery. Never relax a product threshold,
substitute an easier seed or add arbitrary retries to hide a failed premise.

The simulator exercises real signaling, codecs and native agents, but not a
browser's libwebrtc receive path or production Linux socket syscalls. Keep real
browser evidence for SSRC/SDP compatibility and production transport evidence
distinct from simulation. Audio origin/slot checks do not establish perceptual
audio quality. Temporal nonblank checks do not establish smooth playback.

The [native DTLS reconnect exception](native-dtls-exception.md) remains explicit,
narrow and opt-in runnable. Ignored subprocess fixtures are not product
exceptions, and neither category counts as passing proof.
