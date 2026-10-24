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
identify the source by admission, generation, receiver and RTP timestamp. It
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

Encoded polling loss remains diagnostic and invalidates retained source
pairings until new observations authenticate them. It is not native decoder
loss or proof of misrouting. Native callback collection failure still fails
playback. Source samples do not establish complete encoded-stream continuity.

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
