# PulseBeam Agent Guide

## Start with ownership

Before editing, read the `README.md` in every touched immediate child of
`agents/` or `crates/`, then follow its links to local design documents. The
root README is product-facing; module READMEs are the contributor map.

- `agents/` contains the new SANS-I/O and browser client SDK.
- `crates/pulsebeam` is the Linux SFU server and owns its architecture docs.
- `crates/pulsebeam-simulator` owns simulation contracts and end-to-end plans.
- Supporting crates own protocol, routing, runtime, client, CLI, eBPF, and
  deterministic fixture boundaries described in their READMEs.

Keep implementation plans under root `plans/` when work spans modules.

## Non-negotiable server architecture

Read `crates/pulsebeam/docs/thread-per-core.md` before changing server state,
routing, metrics, clocks, or packet flow.

- A shard owns its mutable state and reaches other shards only by owned
  messages. Shared mutable packet runtime is a regression, not a trade-off.
- `Arc`, `Mutex`, `RwLock`, bare atomics, and blocking calls are denied by the
  workspace lint policy. A genuine boundary exception needs a narrow
  `#[allow]` with its reason.
- Several atomic reads never form a coherent snapshot. Publish one complete
  immutable value when fields must agree.
- Anything that may cross a node boundary is a value, never a memory handle.
- The runtime, simulator, agents, and CLI have documented exceptions; an
  exception below the shard model does not license shared shard state.

## Defensive code and tests

- Add `debug_assert!` checks at critical state transitions, timing and buffer
  invariants, encoding offsets, slice boundaries, and assumptions that should
  fail early in simulation without release overhead.
- Validate external input at its boundary. Do not use debug assertions as the
  only defense against malformed network data in release builds.
- Test externally visible properties and invariants, not private steps or
  temporary buffer shapes. Put reusable setup in focused `test_utils` or
  harness modules so scenarios read as domain stories.
- Never use wall-clock sleeps or arbitrary retry loops in tests. Simulation
  time and randomness are process-wide deterministic shims; one seed describes
  one reproducible run.
- A seed-dependent failure is evidence of a behavior difference. Follow
  `crates/pulsebeam-simulator/docs/simulation.md`; do not relax an oracle before
  finding the cause.

## Comments

Default to no comment. Prefer names, structure, and small functions. Remove
nearby narration or stale cross-file claims when editing. Keep only short
comments that explain a non-obvious constraint or workaround, and do not leave
TODOs unless explicitly requested.

## Commands and handoff

Just is the repository task interface; there are no Makefiles. Run focused
Cargo tests or the touched module's `Justfile` while iterating.

Before every handoff:

1. Run `just check`.
2. Run `just test`.
3. Run `just ebpf` when routing classification, envelopes, ufrags, steering,
   eBPF, or its server loader changed.

`just sweep` is exploratory nightly evidence, not a merge gate. Use the
simulator-local `replay` recipe for one failing seed. Never substitute bare
debug-profile `cargo test` for the simulation recipe.
