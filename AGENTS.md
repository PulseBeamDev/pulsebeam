# Agent Development Guide

## Critical Systems & Defensive Assertions

This project is designed to run in simulation (`pulsebeam-simulator`). To catch subtle bugs early under simulated failure conditions, code changes must include extensive defensive assertions.

- **Mandate defensive checks:** Write `debug_assert!`, `debug_assert_eq!`, and `debug_assert_ne!` for all critical state machine transitions, timing invariants, buffer bounds, slice indexing, and network encoding/decoding.
- **Fail early in simulation:** Prefer `debug_assert!` over silent fallback defaults or implicit assumptions. If a state should be unreachable in valid logic, assert it.
- **Zero production overhead:** Use `debug_assert!` variants so these assertions are thoroughly evaluated during test and simulation profile runs without penalizing release builds.
- **Validate input boundaries:** Assert non-empty slices, valid ranges, and buffer offsets at function entry points rather than relying on deep runtime panics.

## Code Comments

Default to writing no comments. Code should be clear enough — through naming, structure, and small functions — that it doesn't need narration.

- Do not add comments that restate what the code already says (e.g. `// increment i` above `i++`).
- Do not add comments explaining routine or self-evident logic.
- Do not add comments that describe a file/module's relationship to other files, crates, or services (e.g. "mirrors the grammar in `other/file.rs`", "keep these two in sync", "intentionally duplicated instead of sharing a dependency"). These go stale the moment one side changes and the comment isn't updated.
- Avoid comments that restate types, parameters, or behavior that will drift out of sync as code changes — anything a future edit could silently invalidate.
- No header/banner comments, no comments marking obvious section boundaries, no restating function signatures in prose.
- Do not leave TODO comments unless explicitly asked to.
- Skip docstrings/JSDoc for simple, self-explanatory functions.
- If a comment feels necessary to explain *why* something non-obvious is done (a genuine workaround, constraint, or trade-off), keep it to one line and place it as close as possible to the code it explains — but treat this as a rare exception, not the norm.
- When editing existing code, remove nearby comments that are redundant, stale, or no longer accurate, rather than leaving them.

When in doubt, leave it out.

## Testing Guidelines

Tests verify fundamental properties and invariants rather than rigid, step-by-step behavior. They must remain stable across internal refactoring and run completely deterministically.

### Property-Focused Over Behavior-Based
- **Assert Invariants, Not Steps:** Focus assertions on high-level properties that must *always* hold true (e.g., "all payload bytes are eventually delivered or reported lost", "state transitions never bypass the handshake", "sequence numbers increase monotonically").
- **Avoid Implementation Coupling:** Do not assert on private internal state, specific sequences of internal method calls, or exact temporary buffer counts. Test against external contracts and end-to-end properties.
- **Resilient to Refactoring:** Refactoring internal algorithms, module boundaries, or data structures should **never** break existing tests as long as the external invariants remain unchanged.

### Modular `test_utils` & Story-Like Readability
- **Reusable `test_utils` Modules:** Expose reusable test harnesses, packet factories, and state assertions in dedicated `test_utils` modules (or `#[cfg(test)]` submodules) so setup logic and scenario assertions are shared cleanly across modules rather than duplicated.
- **Expressive Harnesses:** Abstract low-level setup, socket boilerplate, and network events into domain-specific helpers so test bodies read like a clear narrative (e.g., `sim.connect_peer()`, `sim.partition_network()`, `sim.assert_eventually_healed()`).
- **Self-Describing Scenarios:** A developer reading a test should immediately understand the high-level scenario being verified without having to parse low-level setup or teardown mechanics.

### Zero Flakiness & Determinism
- **No Wall-Clock Delays:** Never use `std::thread::sleep`, real-world timeouts, or arbitrary sleep loops. Use simulated time, deterministic step ticks, or explicit async readiness notifications.
- **Deterministic Simulation:** Ensure any randomized simulation parameters (latency, packet loss, ordering) are seeded so failures are 100% reproducible.

## Preparing Changes

Before considering a change complete, run:

- **Checking:** `cargo check`
- **Unit Tests:** `make test` (or `cargo test`)
- **Simulation Tests:** `cargo test --profile sim --features sim`
- **Test Filtering:** `cargo test <name>`, or for simulation tests: `cargo test <name> --profile sim --features sim`
- **Formatting & Linting:** `make lint`

All of the above should pass cleanly before handing off or committing a change.
