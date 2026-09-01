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
- `src/tests/common/harness.rs` owns automatic discovery/subscription policy, lifecycle commands, reports, and oracles.
- `src/tests/native_runtime.rs` is the narrow native-runtime vertical slice; the authored scenarios exercise the same runtime through the common harness.

## Commands

- Root `just test` runs the committed deterministic suite.
- `just --justfile crates/pulsebeam-simulator/Justfile replay <seed> [filter]`
  reproduces one seed.
- The local `sweep` and `baseline` recipes search seeds and regenerate the
  diffable BWE scoreboard.
