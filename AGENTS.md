# PulseBeam Agent Guide

## Discover before searching

- Start with structured repository truth: `cargo metadata`, the nearest manifest, `cargo tree` when dependency direction matters, `just --list` or the nearest `Justfile`, and rust-analyzer/LSP definition, implementation, and reference queries.
- Use targeted text search only after the owning package or symbol is known. Do not begin with broad filesystem scans, tree dumps, or indiscriminate documentation reading.
- After identifying the owner, read that module's `README.md` and only the linked design documents relevant to the change. Apply the nearest nested `AGENTS.md`.

## Respect executable truth

- Manifests, lints, tests, generators, and Just recipes are authoritative. Do not duplicate their dependency, formatting, generated-code, or command rules here.
- When a lint or repository check rejects a design, follow its reason to the owning contract. Do not bypass architectural checks with broad allows or suppressions; any necessary exception must be narrow and justified beside the code.
- Change generated output through its owning generator or recipe, never by hand.

## Repository convention

- Prefer names, types, structure, and tests over narration. Keep comments only for non-obvious constraints or workarounds, and do not add TODOs unless requested.
- Iterate with the narrowest package-level verification exposed by Cargo or the owning `Justfile`, then use the repository gates exposed by the root `Justfile` before handoff.
