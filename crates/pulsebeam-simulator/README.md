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
