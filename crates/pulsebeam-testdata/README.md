# `pulsebeam-testdata`

Deterministic media fixtures embedded by the native agent and simulator. It
contains Annex-B H.264 streams, Opus samples, decoded reference frames, timing
data, and helpers that expose stable frame identities for quality assertions.

Fixture generation must be deterministic: encoder settings, frame counts, and
the committed manifest together define the corpus. Never replace an asset
without verifying its hash, length, decodability, and expected identity map.

Use `just --justfile crates/pulsebeam-testdata/Justfile --list` for fixture,
quality-corpus, screen-share, analysis, verification, and cleanup workflows.
Normal consumers should use the committed assets and run `cargo test -p
pulsebeam-testdata`.
