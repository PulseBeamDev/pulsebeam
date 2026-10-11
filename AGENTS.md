# PulseBeam Agent Guide

## Discover before searching

* Start with structured repository truth: the nearest manifest, Bazel owner targets, dependency metadata, and and rust-analyzer/LSP definition, implementation, and reference queries when available.
* Use targeted text search only after identifying the owning package or symbol. Do not begin with broad filesystem scans, tree dumps, or indiscriminate documentation reading.
* After identifying the owner, read its `README.md`, the relevant linked design documents, and the nearest nested `AGENTS.md`.

## Documentation and README changes

* **Before creating or editing any `README.md` anywhere in this repository** (including the root), read `docs/AGENTS.md` and the locally bundled topical reference(s) it identifies for the work. Apply its shared developer-documentation standards for accuracy, task-first writing, examples, visuals, and review; external style websites are optional further reading. This also applies to documentation changes outside `docs/`.
* Apply those standards to the README's actual reader: an application developer, SDK integrator, contributor, or package maintainer. A README should be a concise entry point with the right overview, verified setup or contributor commands, and links to detailed task guides, API contracts, and architectural explanations; do not force a README into one page type.
* Keep instructions specific to the owner's package and verified against its current code and build targets. Follow more-specific nested `AGENTS.md` instructions as well. VitePress site configuration and `//docs:check` requirements apply to `docs/**`, **not automatically** to other READMEs.

## Prefer executable truth

* Manifests, Bazel targets, lints, tests, and generators are authoritative. Do not duplicate their dependency, formatting, generated-code, or command rules here.
* When an architectural lint or repository check rejects a design, follow it to the owning contract. Do not bypass it with a broad allow or suppression.
* Any necessary exception must be narrow and justified beside the code.
* Change generated output through its owning source or generator, never by hand.

## Repository convention

* Prefer names, types, structure, and tests over narration. Keep comments for non-obvious constraints or workarounds, and do not add TODOs unless requested.
* Iterate with the narrowest verification exposed by the owning package, then use the repository gates exposed by `BUILD.bazel` before handoff.

## Test routing

* When practical, run the exact affected test, then the nearest owner's fast gate while iterating. Use aggregate slow only for final acceptance; run an exact slow test only while actively debugging that boundary.
* Root `./bazel test //:test` is the complete non-privileged final gate. Run it once, not per slice or review.
* Builders report actual evidence. Reviewers reuse it unless it is missing, stale, contradictory, or needed to reproduce a finding; orchestrators request missing owning evidence rather than duplicating broad runs.
* Final slow evidence is reusable until a repair invalidates it. At renewed final acceptance, rerun affected slow owners, including every affected browser boundary for shared changes, and reuse unaffected passing evidence.
* Privileged perf/network operations and advisory seed sweeps remain outside ordinary test routing.
