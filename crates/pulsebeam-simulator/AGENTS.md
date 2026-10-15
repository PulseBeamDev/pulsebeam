# Deterministic Simulation

- A seed-dependent failure is a reproducible behavior difference, not flakiness. Replay the same seed and find the behavioral cause.
- Committed simulation seeds and plans are gates; exploratory seed search is evidence discovery only. Promote a useful failing seed into the committed suite after fixing the bug.
- Time and randomness are virtualized process-wide by the simulator. Do not add ad-hoc clock or RNG seams solely for tests without first reading the simulator contract.
- Discover replay, sweep, and baseline workflows from the local `BUILD.bazel`; keep command details out of this file.
- Put meaningful repeated full-simulation, multi-seed, codec/native or stress cost in a nested `slow` module; virtual duration alone does not qualify. Cost is separate from fidelity.
- Follow the [contributor testing contract](../../docs/build.md#testing-contract) for proof selection and routing; `#[ignore]` and advisory seed sweeps retain their distinct semantics.
