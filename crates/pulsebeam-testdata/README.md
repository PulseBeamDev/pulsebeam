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

## VP9 layered captures

Run `./bazel test //crates/pulsebeam-testdata:layered_pilot` to check the
S2T1 and L2T1 captures and their recorded decoder observations. This test does
not build a sender or run a decoder. To generate a candidate on Linux x86_64,
run `./bazel build //crates/pulsebeam-testdata:author_layered_pilot`. Review the
resulting `layered-pilot-candidate.tar` before replacing any fixture.

Both captures use the first 12 frames of the repository-owned
`quality_s0_720p.yuv420p.zst` source identified in `src/quality-corpus.manifest`,
box-filtered to 640×360 I420. The pinned upstream libwebrtc encoder and RTP
sender record the original VP9 callback and wire bytes on a virtual clock.
`context.tsv` describes the single RTP binding, and `wire.tsv` indexes RTP and
RTCP bytes by send time. `encoded.tsv` records callback identity and buffer
operations before packetization. `observations.json` maps both views and records
FFmpeg library versions, strict software-decoder settings, presentation times,
and SHA-256 digests of each decoded plane without stride padding or scaling.
The authoring decoder comes from `tools/host/flake.lock`.

Removing the first lower-layer unit's packets leaves every S2T1 upper picture
unchanged. For L2T1 the same omission removes an observed reference: strict
decoding rejects the stream and produces no picture. Corrupting the first
retained upper unit is rejected for both modes. These captures qualify that
experiment, not PulseBeam forwarding or recovery timing.

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
