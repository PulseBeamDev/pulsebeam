# `pulsebeam-simulator`

Deterministic end-to-end verification of the PulseBeam SFU and native agents.
Each authored plan runs in an isolated process with virtual time, seeded process-
wide randomness, shaped networks, real signaling, and invariant-based oracles.

A seed-dependent failure is a reproducible behavior difference, not flakiness.
Do not weaken an oracle or add wall-clock waiting to make a plan pass.

Read the [simulation contract](docs/sim.md) before changing the harness and the
[testing guide](docs/simulation.md) before adding plans, randomness, thresholds,
or failure injection.

## Contributor map

- `src/tests/common/client.rs` adapts scenario operations to complete native-agent desired-state revisions and coherent snapshots.
- `src/tests/common/media.rs` owns deterministic encoded video and audio source behavior used only by simulation.
- `src/tests/common/harness.rs` owns automatic discovery/subscription policy, lifecycle commands, reports, and bounded convergence checkpoints.
- `src/tests/common/oracle.rs` models plan-derived intent and compares coherent control, shard, and client observations.
- `src/tests/native_runtime.rs` is the narrow UniFFI-facing native-runtime vertical slice; the authored scenarios exercise the same runtime through the common harness.

## Native playback contracts

Use `Participant::with_native_playback()` for user-visible playback scenarios.
It observes actual native I420 and uses authenticated encoded samples only to
identify the source by admission, generation, full native track identity and RTP
timestamp. It
does not run an independent decoder or issue keyframe requests because a
polling observer fell behind. Native decoder feedback remains authoritative.

- `BeginNativePlayback` opens a measurement window without waiting. Set
  `after_pause` only after an intentional publication absence; it excludes
  that absence from cadence, not accumulated errors or collector loss.
- `CheckNativePlayback` requires new nontrivial native pictures, changing
  content, the requested dimensions, authenticated current-source samples and
  an actual SDK decoder report. Camera output must be at most 250 ms old with
  no inter-output gap above 500 ms. `DesktopUpdates` uses the existing
  five-second visible-output bound rather than imposing camera cadence.
- `WaitForNativeVideo` bounds first visibility from the step's start. It
  requires new source-authenticated output, current native pictures and exact
  room convergence. A subsequent playback window is a separate, explicitly
  timed continuity claim. Slot replacement also checks the original native
  receiver identity.

Playback source correlation joins actual native ordered-encoded and I420
callbacks independently of best-effort polling retention. Polling loss remains
diagnostic and cannot erase those callback facts. Missing native identity or
metadata revokes source pairings until new actual observations authenticate
them; pre-loss native output is not reused. Native callback collection or
conversion failure still fails playback. Source samples do not establish
complete encoded-stream continuity.

Keep `with_encoded_and_native_decoded_video()` and encoded quality assertions
for scenarios specifically testing encoded forwarding or independent codec
correctness. Those probes retain their own loss/error budgets. Encoded quality
assertions reject native-playback-only observations.

These checks establish native decoded output, not UI presentation. Browser and
production integration evidence remain distinct. A real active-camera freeze
still fails; redesigning the oracle does not qualify unresolved recovery or
publication failures.

## Build profile

Simulator runners apply the workspace's Cargo `sim` optimization and safety
settings to the full target-configured Rust dependency closure. Proc macros and
build scripts keep their normal execution configuration, and other owners keep
their normal build configuration. Target-local optimization alone does not
optimize runtime dependencies.

## Commands

See the [contributor testing contract](../../docs/build.md#testing-contract) for
workspace gates and evidence reuse. Native media/topics/reconnect/close runs
in the ordinary fast gate and replay. The
[historical native-str0m DTLS exception](docs/native-dtls-exception.md) is retired;
focused controlled proof does not replace shared browser or production evidence.

- `./bazel run //:replay -- --seed <seed> --exact --filter <name>` reproduces one case.
  Add `--jobs <count>` to select workers or `--show-output` for grouped full traces;
  `RUST_LOG` controls diagnostic detail. Each invocation prints an owned bundle
  directory. Bazel tests retain it in undeclared outputs; standalone replay can
  select a persistent parent with `--artifacts-dir /absolute/path/outside/checkout`.
- `./bazel run //:sweep -- --seeds <count> --from-seed <first> --filter <name>`
  searches replayable seeds outside merge gates.
- `./bazel run //:replay -- --replay-bundle /absolute/path/to/run-or-case`
  replays retained selection, settings and captured inputs on the recorded source
  candidate. Move the whole run directory when transporting a case bundle.
- `./bazel run //:scoreboard` explicitly regenerates the diffable BWE scoreboard.

See [failure retention and replay](docs/simulation.md#retain-and-replay-generated-failures)
for bundle contents, source identity requirements and retention limits.

## Calibrate public-contract detectors

Run the bounded catalog explicitly:

```sh
./bazel test //crates/pulsebeam-simulator:calibration --nocache_test_results
./bazel run //crates/pulsebeam-simulator:calibration_replay -- --replay-bundle /absolute/path/to/run-or-case
```

The weekly/manual `proof-calibration.yml` workflow runs this owner and retains
nonzero failures. Mutant builds are not dependencies of root fast, shipping
artifacts or complete acceptance. Only cheap transform/classification contracts
run in ordinary tool checks. The three public controls themselves run in normal
simulation selection. Compare uncached execution (`wall_seconds` per case) with
Bazel's separate build/cache cost when changing these workloads.

| Defect | Controlled premise | Public rejection |
| --- | --- | --- |
| Local fanout serves only its first recipient | One shard, publisher then two subscribers | Exact source/topic/payload receipt missing at an intended subscriber after a bounded virtual deadline, with positive delivery at the other |
| Remote fanout serves only its first destination | Three shards, sequential admissions with partition size one and round-robin placement | The same per-recipient public receipt contract across both destination shards |
| Negative materialization acknowledgement treated as success | Existing one-shot materialization failure consumed exactly once | Actual admission HTTP 201 instead of 503, observed through the SDK caller's HTTP host |

Controls use the real native SDK and SFU without a private observer. The
materialization control additionally requires SDK retry, successful delivery to
the recovered subscriber, and continued unrelated delivery. Fault bookkeeping
only proves controlled exercise. Send admission and SDK topic readiness never
count as recipient delivery. These topic controls prove forwarding and admission,
not media decoding, presentation, all QoE guarantees or overall test adequacy.

`tools/calibration-transform.py` owns three finite, exact-context transforms.
Bazel generates complete source/compile-data layouts and substitutes exactly one
SFU library into each separately linked, simulation-profile executable. Source
context must match once or generation fails stale. To maintain a site, review
its new context and intended semantic defect together, then run the cheap
`//tools:calibration_contract` and the explicit calibration owner. Do not add
arbitrary operators, edit worktrees or patch generated output.

Calibration retains candidate identity, executable target/checksum, transform
checksum and per-file hashes, case/seed, reached premise, requests, receipts,
first public violation and full trace for controls as well as variants. Version
2 replay bundles belong to `calibration_replay`; ordinary replay rejects them.
Unmodified version 1 bundles retain their ordinary passing-test interpretation.
Replay requires the recorded actual candidate, finite transform and executable.
Moving a bundle does not package dirty source or the toolchain.

Classification distinguishes `control-pass`, `control-failure`,
`detected-public-defect`, `survived-defect` and `infrastructure-failure`. Only a
reached premise and its attributed public violation detect one of these defects.
A no-op or repaired variant survives and fails calibration. Missing evidence,
setup/build failure, unrelated crash, stale context and host timeout are not
detections. Production-inline invariant rejection is a separate boundary, never
an observed SDK violation: this catalog currently requires public detection and
does not infer invariant attribution from panic text or source lines.

BUGGIFY-style injection is allowed, but private structural invariants belong
inline in production owners via `assert!` or `debug_assert!`, executed by the
simulation profile. Never add a private snapshot, diagnostic export or production
observation knob to help a variant fail. A new public observation needs a concrete
production consumer/action, insufficient existing alternatives, a minimum stable
contract, and justified compatibility, portability, privacy and maintenance
costs. A public wrapper over private state or testing convenience is insufficient.
