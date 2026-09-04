# PulseBeam Agent Guide

## Discover before searching

* Start with structured repository truth: `cargo metadata`, the nearest manifest, `cargo tree` when dependency direction matters, `just --list` or the nearest `Justfile`, and rust-analyzer/LSP definition, implementation, and reference queries when available.
* Use targeted text search only after identifying the owning package or symbol. Do not begin with broad filesystem scans, tree dumps, or indiscriminate documentation reading.
* After identifying the owner, read its `README.md`, the relevant linked design documents, and the nearest nested `AGENTS.md`.

## Prefer executable truth

* Manifests, lints, tests, generators, and Just recipes are authoritative. Do not duplicate their dependency, formatting, generated-code, or command rules here.
* When an architectural lint or repository check rejects a design, follow it to the owning contract. Do not bypass it with a broad allow or suppression.
* Any necessary exception must be narrow and justified beside the code.
* Change generated output through its owning source or generator, never by hand.

## Repository convention

* Prefer names, types, structure, and tests over narration. Keep comments for non-obvious constraints or workarounds, and do not add TODOs unless requested.
* Iterate with the narrowest verification exposed by the owning package or `Justfile`, then use the repository gates exposed by the root `Justfile` before handoff.
