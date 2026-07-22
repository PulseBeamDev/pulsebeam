# Agent Development Guide

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

## Preparing Changes

Before considering a change complete, run:

- **Checking:** `cargo check`
- **Test:** `make test`
- **Test filter:** `cargo test <name>`, or for the `pulsebeam-simulator` crate: `cargo test --profile sim --features sim`
- **Formatting & Linting:** `make lint`

All of the above should pass cleanly before handing off or committing a change.
