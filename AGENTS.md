# PulseBeam Agent Guide

## Discover before searching

* Start with structured repository truth: the nearest manifest, Bazel owner targets, dependency metadata, and and rust-analyzer/LSP definition, implementation, and reference queries when available.
* Use targeted text search only after identifying the owning package or symbol. Do not begin with broad filesystem scans, tree dumps, or indiscriminate documentation reading.
* After identifying the owner, read its `README.md`, the relevant linked design documents, and the nearest nested `AGENTS.md`.

## Documentation and README changes

* When writing or editing `docs/**`, any `README.md`, or other developer-facing documentation, follow `docs/AGENTS.md` and any nearer local instructions. Apply VitePress-specific checks only to `docs/**`.

## Prefer executable truth

* Manifests, Bazel targets, lints, tests, and generators are authoritative. Do not duplicate their dependency, formatting, generated-code, or command rules here.
* When an architectural lint or repository check rejects a design, follow it to the owning contract. Do not bypass it with a broad allow or suppression.
* Any necessary exception must be narrow and justified beside the code.
* Change generated output through its owning source or generator, never by hand.

## Repository convention

* Prefer names, types, structure, and tests over narration. Keep comments for non-obvious constraints or workarounds, and do not add TODOs unless requested.

## Testing

Follow [the contributor testing contract](docs/build.md#testing-contract) for proof selection, gate routing, isolation, and evidence reuse. Executable owner targets remain authoritative.
