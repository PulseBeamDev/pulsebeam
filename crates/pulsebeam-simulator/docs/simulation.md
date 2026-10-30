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

`ApplicationStateOracle` models participants, publications and complete receive/topic
intent from plan operations. Admission observations bind opaque IDs to plan
identities; server mappings and forwarded packets are not the source of
expected intent.

Compare application inputs with documented SDK snapshots and actual recipient
output after bounded protocol progress. Active admission correlation uses the
HTTP offer's ICE identity and active native statistics from the pinned backend;
missing or ambiguous identity fails instead of selecting the latest HTTP reply.
Private control/shard projections are not correctness observations. Their
structural invariants belong inline in their production owners.

Automatic audio must cover every live source when it fits, or fill the remaining
unpinned slots with live sources under contention. Packet timing, power decay
and slot immunity can change the contended subset, so the common oracle does not
invent a unique ordering. Explicit speaker-selection assertions retain the
scenario's loudness and origin claims.

At a settled publication-discovery checkpoint, also require the applicable
full-client convergence: one catalog entry does not establish every client's
current roster or bindings. Keep that work inside the existing bounded deadline,
not a fixed extra sleep. Do not make global quiescence a prerequisite for
established service while unrelated intent or fault work is still outstanding.

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
`.regressions` artifact. The runner gives each case its own output directory;
new minimized failures are appended there, never written into tracked source.
The printed `[simulation-regressions]` line identifies the artifact.

Every owner test, replay and sweep invocation prints `[simulation-artifacts]`
with its unique owned run directory. Bazel test actions own this directory under
`TEST_UNDECLARED_OUTPUTS_DIR/simulations/`. Standalone commands use a temporary
parent unless you pass `--artifacts-dir /absolute/path/outside/checkout`; the
runner creates a unique child there, including for overlapping invocations.
Keep output outside the checkout, or in an ignored directory, so generating
evidence does not itself change the recorded source candidate.

The run retains:

- `manifest.json`: source revision, dirty status and fingerprint, selection,
  explicit replay settings, supplied-input checksum, discovery results and
  per-case outcomes. A null case seed means authored defaults, not seed zero.
- `replay.json`: the run selection and replay inputs. Each `case-NNNN/` has its
  own exact-case `replay.json` and `result.json`; per-case results also remain
  available if a campaign is interrupted before its final manifest update.
- `inputs/supplied.regressions`, when provided: captured before discovery and
  consumed from that copy by every child, not from the original external path.
- For each failed case, `trace.log` with complete combined stdout/stderr and
  `outputs/` containing the emitted family corpus and any other case artifacts.
  Failed discovery retains its trace and owned discovery outputs. Launch errors
  and host timeouts retain available diagnostics; timeouts kill the process group.

The runner measures host work with its own monotonic clock, not the child's
virtual process clock. Each discovery record has `wall_seconds`; each case has
child `wall_seconds` and separate `evidence_wall_seconds`. The manifest's
`timings` separates discovery, summed child execution, campaign wall time,
reporting and total runner time. Campaign time includes scheduling, evidence
processing and reporting, which can overlap other children. These values are
not additive CPU attribution and do not include Bazel build/cache time.
Compare uncached child execution under fixed candidate, selection, settings,
seed and jobs with repeated observations; report host variability rather than
asserting a CI speed threshold.

Success traces and emitted outputs are discarded after concise reporting.
`--show-output` prints complete grouped traces, including successes. The runner
retains only this invocation's metadata, replay inputs and failure diagnostics;
it does not discover or collect arbitrary temporary files. Remove retained run
directories when no longer needed. Unavailable required evidence or candidate
metadata is an infrastructure failure, never passing product proof.

To replay a transported failure, copy the **whole run directory**, including
`inputs/`, to the replay host. Use the recorded actual source candidate and the
supported toolchain, then point at the run or one case:

```sh
./bazel run //:replay -- --replay-bundle /absolute/path/to/run/case-0000
```

Replay restores the recorded selection, explicit `RUST_LOG` and proptest
settings, including their absence, and verifies the captured corpus checksum.
The supported Bazel build boundary supplies source provenance as declared data;
the hermetic runner does not read `.git`. Replay rejects a different revision,
dirty state or fingerprint. A clean revision identifies source; a dirty
fingerprint only attributes local sources and cannot reconstruct them from the
base commit. Bundles do not package sources, binaries or toolchains. RNG states
remain subject to the current strategy; bundles do not preserve historical
semantic scenarios that the case did not serialize.

To replay an emitted minimized corpus rather than the originally captured input,
provide its absolute path while retaining the case and whole-run seed:

```sh
PULSEBEAM_SIM_REGRESSIONS=/absolute/path/to/family.regressions \
  ./bazel run //:replay -- --seed <seed> --exact --filter <case>
```

Review retained RNG states and promote useful regressions into the committed
corpus or a focused authored case. The isolated persistence proof exercises both
committed input under a current strategy and a newly minimized artifact in a
fresh process with no novel cases. Nightly seed sweeps upload the owned run
evidence and summary even on failure. They preserve non-zero results but remain
advisory searches, not merge correctness gates.

## Replay semantic intent histories

The required intent-history family preserves a 24-case novel lifecycle budget,
replays retained corpus inputs, and forces outstanding receive replacement and
withdrawal, generation replacement with held traffic, controller stall, and
failed materialization with a driver-planned admission retry. Each concrete
program runs in a fresh process so earlier worlds cannot affect its identities
or randomness. Counts distinguish novel, retained and reduction executions from
applied operations and reached claims.

Failures retain `*.original.history` and `*.reduced.history` beside the runner
traces. Failed-case `artifact_checksums` covers all retained owned output
files, including original semantic inputs and any supplied nearby variations.
Only the unambiguous top-level `*.reduced.history` selects a concrete replay;
scratch files are retained diagnostics, not replay selectors. Ambiguous reductions
or unavailable artifact checksums fail evidence retention as infrastructure;
`evidence_error` records that problem without replacing the original child exit,
launch or timeout failure. Artifact links outside the owned output boundary are
not followed. Replay requires the current selection fields and a supported
bundle format; incompatible evidence is rejected before discovery.
These are concrete programs, not RNG states. They contain the
whole-run seed, replay selection, symbolic operations and actual phase/fault
witnesses. The failed case's portable replay bundle selects the reduced concrete
input and checks its checksum and candidate identity. A supplied
`PULSEBEAM_SIM_HISTORY` input is also captured before discovery. To replay a
concrete artifact directly without invoking the generator:

```sh
PULSEBEAM_SIM_HISTORY=/absolute/path/to/case.reduced.history \
  ./bazel run //:replay -- --seed <recorded-seed> --exact \
  --filter tests::histories::required_intent_histories
```

Unsupported versions, instructions, invalid lifecycle operations and mismatched
seeds fail rather than being silently reinterpreted. Reduction keeps valid
programs with the same reached claim and failure phase; it does not count
filtered no-ops as exercise. The bounded reducer retains the best candidate it
actually evaluated, not a claim of global minimality.

Transition checks permit outstanding intent and lagging shard projections.
Retirement fences use each owner's generation rather than a global atomic
snapshot. After the final repair/recovery command, exact convergence and fresh
decoded intended-origin delivery must complete within the existing 30-second
simulated recovery window. Crash retirement remains bounded from the crash by
20 simulated seconds. Holds and controller stalls last at most five simulated
seconds. These are recovery proof bounds, not new media-quality SLOs.

### Cross-lane inputs and bounded discovery

Concrete inputs declare source ownership, data ownership/delivery modes and
audio requests. Audio selects pinned speakers and automatic fill with supported
adaptive playout; fresh native decode is a separate operation, not inferred from
Mapping. The native profile does not negotiate audio RTP playout-delay. Fixed
audio requests are rejected before changing accepted state, rather than dropping
media or treating a minimum-only setter as a full min/max policy. The existing
native runtime world checks per-track and selector-policy rejection followed by
real audio decode; fixed-buffer service proof remains on video. Known quiet fixtures use fresh decoded sample progress without a
universal energy floor or invented PCM source attribution.

Pressure is bounded application work: four actual desired-state submissions
(coalescing is allowed), or eight actual native PCM producer submissions with
every public result settled as accepted or backpressured. It is not a claim
that four packets reached the wire or that a private queue saturated. These
operations can precede repair while independent media/data must still progress.
Fresh-process replay covers a composed cross-lane program and the exact retained
admission-replacement regression.

The required gate retains its four forced cases, eight corpus-derived cases and
24 novel cases. Advisory search is off by default and never rerolls a failed
world. Opt in with one seed:

```sh
./bazel run //:replay -- --seed 17 --exact \
  --filter tests::histories::required_intent_histories \
  --history-search compare --show-output
```

Each arm evaluates 24 histories with 96 deterministic proposals, at most 32
operations per history and eight failure reductions. The main operation budget
is 768; including reductions it is at most 1,024. Both arms use the same seed
and generator budget. Guided selection adds deterministic novelty selection and
bounded mutation using only executed public-input/fault-phase features; it has
no private state, coverage counters or observer API. The unguided arm selects
without feedback. Search mode is an explicit captured/replayed runner selection,
not an ambient environment setting. Exact failures and reductions use the same
checksum-verified concrete replay path as the required gate.

Output records actual operations, reductions, reached public claims and novelty.
Only the outer runner measures host wall time; clocks inside simulation are not
host timing evidence. Input novelty differences are not proof of system coverage,
correctness or performance benefit. Use runner child timings separately from
Bazel compilation/cache cost; the opt-in comparison is not a new merge matrix.

## Check ordered-topic reconfiguration

The data-channel scenarios cover publisher-first and subscriber-first declarations
on already-connected native agents, then exact ordered delivery before and after
publisher rejoin. `LocalNodeSim::without_initial_topics()` keeps declarations out
of the initial offer so these cases exercise real transport reconfiguration.
Both intended receivers must receive the complete sequence; the publisher and
unsubscribed bystander must receive none. These signaling/placement cases use a
clean fiber profile; the separate reliable-data impairment case retains seeded
loss and feedback coverage.

Use ordinary case-and-seed replay through `//:replay`. These scenarios use stock
Turmoil and crypto, not exhaustive runtime-schedule exploration or byte-for-byte
cryptographic replay. Named internal-fault scenarios remain in `connectivity.rs`.

## Keep recovery and fidelity claims honest

Recovery plans must force each required fault site, prove that it fired and
check post-fault atomicity and convergence. Branch counters alone establish
neither recovery nor continued delivery. Never relax a product threshold,
substitute an easier seed or add arbitrary retries to hide a failed premise.

For a fixed recovery window, put `Step::Run` immediately after repair, then use
`CheckVideoQualityInterval` and `CheckVideoDecodedFromInterval` for each intended
receiver and origin. Both use baselines captured at the start of that run, so
pre-repair decoded frames cannot establish recovery. Keep a positive decoded
baseline before the fault. `CheckVideoNotReceivedFrom` checks cumulative received
and decoded frames, suitable for foreign origins that must never reach a viewer.

The simulator exercises real signaling, codecs and native agents, but not a
browser's libwebrtc receive path or production Linux socket syscalls. Keep real
browser evidence for SSRC/SDP compatibility and production transport evidence
distinct from simulation. Audio origin/slot checks do not establish perceptual
audio quality. Temporal nonblank checks do not establish smooth playback.

The [historical native-str0m DTLS exception](native-dtls-exception.md) is retired
after replacement libwebrtc lifecycle proof. The native case is required by
ordinary fast, replay and sweep selection. Ignored subprocess fixtures remain
separate from product acceptance; focused native proof does not waive shared
browser or production checks.
