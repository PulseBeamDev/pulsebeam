# Deterministic Simulation

- A seed-dependent failure is a reproducible behavior difference, not flakiness. Replay the same seed and find the behavioral cause.
- Do not make a plan pass by adding wall-clock sleeps, arbitrary retries, changing seeds, or weakening an oracle. Oracles should express externally visible outcomes or architectural invariants.
- Committed simulation seeds and plans are gates; exploratory seed search is evidence discovery only. Promote a useful failing seed into the committed suite after fixing the bug.
- Time and randomness are virtualized process-wide by the simulator. Do not add ad-hoc clock or RNG seams solely for tests without first reading the simulator contract.
- Discover replay, sweep, and baseline workflows from the local `Justfile`; keep command details out of this file.
- Simulation is fast by default. Put meaningful repeated full-simulation, multi-seed, or stress wall-clock cost in a nested `slow` module; virtual duration alone does not qualify.
- Iterate with an exact/narrow test and then the local fast suite. Run the aggregate slow suite only as a final check; `#[ignore]` and seed sweeps retain their distinct semantics.
