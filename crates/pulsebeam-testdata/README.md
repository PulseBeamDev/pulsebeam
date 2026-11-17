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

## Common-source comparison inputs

Use `QualitySourceSequence::decode(source)` for a bounded six-second, 30fps raw
source. It decompresses the existing 1280x720 decoded reference, whose encoded
and compressed-reference hashes and generator versions are in
`src/quality-corpus.manifest`. This is synthetic, previously H.264-compressed
content, not pristine camera footage. No fixture regeneration is needed.

`capture(index)` exposes one source/epoch/frame identity and integer run-relative
capture time, independent of how many representations are produced. It does not
loop past capture 179. The two 90-frame corpus epochs remain explicit; callers
own any additional source-lifetime identity. `representation(layer)` derives
320x180, 640x360 or 1280x720 tightly packed I420 from that same capture for fixed
encoding or live encoder input. These are not the independently encoded corpus
layer assets. Existing `quality_corpus_video` consumers remain unchanged.

The common quality view is 320x180 I420. `quality_common_view` projects actual
decoded output of any rung into this view, and `capture.common_view()` supplies
its canonical source reference. Policy `QUALITY_COMMON_VIEW_POLICY` specifies
integer box averaging independently in Y/U/V with floor rounding. Its source
is the decoded 720p fixture, including its original text labels. Labels therefore
remain source content, not evidence of the selected output rung. The independent
180p/360p assets have post-scale labels and are **not** alternative common-view
references. Re-encoding derived inputs is an additional codec generation, which
must be disclosed in experiment results.

These helpers establish input identity and shared pixel semantics, not native
encoder output identity, feedback response or transport delivery. Those require
actual encoder/media observations in the experiment owner. Report presented
quality against the current capture reference, including when an older decoded
image is held; keep blank time and decoded-only quality separate.
