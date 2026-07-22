# Agent Development Guide

## Code Comments

Use minimal comments. Follow these rules:

- Do not add comments that restate what the code already says (e.g. `// increment i` above `i++`).
- Do not add comments explaining routine or self-evident logic.
- Only comment when the *why* isn't obvious from the code itself — non-obvious trade-offs, workarounds for external quirks, or business rules that can't be inferred from context.
- Prefer clear naming and small, well-structured functions over comments that explain unclear code. If code needs a comment to be understood, consider rewriting it instead.
- No header/banner comments, no comments marking obvious section boundaries, no restating function signatures in prose.
- Do not leave TODO comments unless explicitly asked to.
- Skip docstrings/JSDoc for simple, self-explanatory functions. Use them only for public APIs or functions with non-obvious behavior, parameters, or return values.
- When editing existing code, remove redundant comments nearby rather than adding to the clutter.

Default to no comment. Add one only if it earns its place.

## Preparing Changes

Before considering a change complete, run:

- **Checking:** `cargo check`
- **Test:** `make test`
- **Test filter:** `cargo test <name>`, or for the `pulsebeam-simulator` crate: `cargo test --profile sim --features sim`
- **Formatting & Linting:** `make lint`

All of the above should pass cleanly before handing off or committing a change.
