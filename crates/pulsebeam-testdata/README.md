# `pulsebeam-testdata`

Deterministic media fixtures embedded by the native agent and simulator. It
contains Annex-B H.264 streams, Opus samples, decoded reference frames, timing
data, and helpers that expose stable frame identities for quality assertions.

Fixture generation must be deterministic: encoder settings, frame counts, and
the committed manifest together define the corpus. Never replace an asset
without verifying its hash, length, decodability, and expected identity map.

Consumers use committed assets, declared as Bazel compilation inputs.
`./bazel test //crates/pulsebeam-testdata:unit_tests` validates corpus identity,
hashes, and frame metadata. Corpus authoring scripts are not required for
normal development or acceptance; never regenerate fixtures as package preparation.
