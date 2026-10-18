# Deterministic Simulation

- A seed-dependent failure is a reproducible behavior difference, not flakiness. Replay the same seed and find the behavioral cause.
- Committed simulation seeds and plans are gates; exploratory seed search is evidence discovery only. Promote a useful failing seed into the committed suite after fixing the bug.
- Time and randomness are virtualized process-wide by the simulator. Do not add ad-hoc clock or RNG seams solely for tests without first reading the simulator contract.
- Discover replay, sweep, and baseline workflows from the local `BUILD.bazel`; keep command details out of this file.
- End-to-end scope and generated coverage must stay very fast. Optimize avoidable media, setup, polling, reporting and repeated-world work while retaining real production logic, deterministic replay and complete convergence/origin oracles. Do not substitute mocks or weaker component-only proof to claim a speedup.
- Measure uncached execution before and after workload changes. Virtual time alone does not make a test fast; moving it to `slow`, reducing meaningful cases or adding workers does not fix its cost. Reserve `slow` for justified irreducible boundary work, not accumulated harness waste.
- Follow the [contributor testing contract](../../docs/build.md#testing-contract) for proof selection and routing; `#[ignore]` and advisory seed sweeps retain their distinct semantics.
