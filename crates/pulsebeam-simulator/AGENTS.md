# Deterministic Simulation

- A seed-dependent failure is a reproducible behavior difference, not flakiness. Replay the same seed and find the behavioral cause.
- Do not make a plan pass by adding wall-clock sleeps, arbitrary retries, changing seeds, or weakening an oracle. Oracles should express externally visible outcomes or architectural invariants.
- Committed simulation seeds and plans are gates; exploratory seed search is evidence discovery only. Promote a useful failing seed into the committed suite after fixing the bug.
- Time and randomness are virtualized process-wide by the simulator. Do not add ad-hoc clock or RNG seams solely for tests without first reading the simulator contract.
- Discover replay, sweep, and baseline workflows from the local `Justfile`; keep command details out of this file.
