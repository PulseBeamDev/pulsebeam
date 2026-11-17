# Corpus RTC uplink comparison

The ordinary runner compares six adaptive uplinks using one corpus capture
ledger and one experimental decoder/selection/presentation pipeline. The
[comparison contract](./comparison-contract.md) defines the measurement and
migration boundaries. This is a system comparison, not an isolated controller
benchmark or a browser compatibility qualification.

Benchmark results are local generated output, not checked-in fixtures. The
bounded study produces 72 primary runs and six scene-sensitivity witnesses.
Inspect individual-run scalars, normalized traces and source provenance together;
a fresh checkout contains none of these generated files. See
[reproduction and local results](#reproduce-and-inspect-local-results).

Measurements apply only to this bounded uplink implementation. They do not
establish native parity, a universal transport ranking or an isolated
congestion-controller comparison.

## Run from a contributor checkout

Use the repository's normal Bazel/native SDK prerequisites:

```sh
./bazel test //crates/pulsebeam-transport:experiment_controls
./bazel run //crates/pulsebeam-transport:webtransport_poc -- --scenario=clean
```

The second command emits JSONL: one `rtc-uplink-v1` metadata object followed by
individual run objects. With no filters it runs four conditions, one seed and
all six arms. These are bounded six-second source lifetimes with a common
1.5-second drain, not convergence or steady-state claims.

| Arm filter | Sender/controller | Transport |
| --- | --- | --- |
| `native_screamv2` | Native raw-video simulcast, observed native SCReAMv2 | Native SRTP/UDP |
| `native_gcc` | Native raw-video simulcast, GCC with SCReAM disabled | Native SRTP/UDP |
| `udp_scream` | Custom aggregate source, Rust SCReAMv2 | Production certificate-pinned DTLS/UDP |
| `quic_outer` | Same custom source and outer Rust SCReAMv2 | Noq `OuterControlled` |
| `quic_cubic` | Same custom source and outer Rust SCReAMv2 | Noq CUBIC |
| `quic_bbr3` | Same custom source and outer Rust SCReAMv2 | Noq BBR3 |

Supported options are `--scenario=clean|capacity_step|loss_1pct|rtt_160ms`,
`--arm=<table value>`, `--seeds=1..3` and `--source=0|1`. Each option takes one
value, not the literal pipe-separated alternatives. The old diagnostic, bare
QUIC, fixed-input ranking and dropper-sensitivity CLI modes have been removed.
The separate full-native single-stream codec sanity control remains test-only.

CUBIC and BBR3 are inner controllers, not replacements for the outer SCReAM.
`OuterControlled` retains QUIC ACK/loss/recovery and permissive finite inner
ceilings. Noq is not browser WebTransport or HTTP/3. No browser ECN or L4S claim
is made.

## Interpret local results

Source 0 uses seeds 1, 2 and 3 for each of the four conditions. Source 1 has
only a clean seed-1 sensitivity witness; do not pool it with source 0 or
extrapolate its behavior under impairment. Both sources are synthetic, not
natural camera footage.

Each run offers 180 captures over six seconds. Timely means decoder-ready
within 180ms on any rung. Age is the duration-weighted mean age of the displayed
capture during nonblank intervals. Freeze duration is the total duration of
held runs lasting at least 200ms. Read presented PSNR alongside blank duration:
it excludes blank intervals. Wire costs include attempts dropped by the model
and are not useful payload throughput.

Compare per-run results rather than pooled ratios or a single quality ranking.
Fewer wire bytes can accompany fewer timely captures or longer freezes. Three
seeds expose variation, not a population distribution. Seeds fix the modeled
network's random draws, not protocol-internal entropy; repeating a seed need not
produce identical results. Different packetizations do not experience identical
semantic losses. These six-second lifetimes do not establish convergence or
steady-state behavior.

## Source, observation and quality

`corpus.rs` uses the existing decoded 1280x720 I420 corpus, with 180 offered
captures at 30fps, two 90-frame epochs and no implicit replay loop. A
scope/capture CRC watermark is applied once before deriving q/h/f inputs and the
320x180 quality reference. Alternative representations never multiply the
capture denominator. The source/reference policies and corpus manifest are
recorded in metadata.

All arms start with one 300kbit/s aggregate path target and 200/600/1400kbit/s
rung ceilings. Native retains its actual encoder allocation/adaptation. The
custom source activates affordable rung minima, then assigns surplus low-rung
first; its versioned policy is not a rate-distortion optimizer. The source
counts pauses, pre-encode backpressure and real encoder omissions separately.
Produced pictures are retained through blocked sends without caller expiry.
Native per-capture encoded production and ownership are null with reasons where
the raw-source API does not expose them.

Native q/h/f media enters receiver-only production RTC with negotiated TWCC.
Actual authenticated RID/SSRC associations select a rung. Genuine RTCP keyframe
requests repair lost dependencies. Custom datagrams use experimental framing,
encoded-output sequence numbers and reverse-path keyframe requests. Unencoded
capture gaps are not encoded dependency gaps. Both paths use the same actual
H264 decoder and callback adapter. Packet completion and send acceptance are
not decoder-ready observations or delivery proof.

`measurement.rs` checks decoded content identity, geometry and actual RTP
progression. Direct codec timestamps use the 90kHz capture clock; native output
is checked against its observed millisecond-quantized capture clock. Decoder
observations occur at the end of the modeled 1ms host turn; codec CPU latency is
not modeled. This is not a physical glass-to-glass latency measurement.

The all-offered timeline samples every 10ms, with a 180ms decode deadline and a
200ms held-run freeze threshold. Drain observations can classify late/missing
captures, but cannot rewrite presentation or extend the six-second horizon.
Held-image error uses the current offered reference, not the image's original
reference. Blank duration is reported separately, not assigned invented black
pixels. PSNR aggregates squared error and sample/time weights before taking the
logarithm. I420 samples have their natural 4:1:1 Y:U:V counts. Decoded-
representation PSNR is conditional diagnostic evidence, not a user-experience
ranking. Exact duration-weighted integers are decimal strings in JSON.

Run objects retain offered, actual decoded and selected-tick traces, with
reference errors. Wire costs count attempted, scheduled and dropped bytes by
direction, including 28-byte IPv4/UDP overhead; scheduled bytes include packets
still pending at cutoff. Caller admission and terminal accepted-prefix/queued-
tail dispositions remain separate from decode and presentation. A trace ending
before the offered horizon is unqualified and has unavailable timeline metrics,
not an extrapolated successful tail.

## Reproduce and inspect local results

Keep generated results out of Git. The experiment-local `.gitignore` excludes
`results.json`, `results.meta.json` and `results.traces.jsonl.gz`. Raw stdout and
verifier logs should also stay outside the checkout. Do not force-add generated
results or copy their tables into this guide.

From a clean source revision, run the bounded study with:

```sh
./bazel run //crates/pulsebeam-transport:webtransport_poc -- --seeds=3 --source=0 > /tmp/rtc-study-source0.jsonl
./bazel run //crates/pulsebeam-transport:webtransport_poc -- --seeds=1 --source=1 --scenario=clean > /tmp/rtc-study-source1.jsonl
```

The packer additionally requires the actual passed verifier's `evidence.json`
for those two commands, matching the clean measured source revision. With that
receipt available, replace the placeholder path below and pack the local files:

```sh
python3 crates/pulsebeam-transport/experiments/results.py pack /tmp/rtc-study-source0.jsonl /tmp/rtc-study-source1.jsonl /path/to/passed-study-evidence.json
```

Use Python 3.11 or newer. Packing validates the exact 78-run set and writes the
three ignored artifacts together. `results.json` preserves individual-run
scalar rows. `results.traces.jsonl.gz` preserves normalized metadata/run JSONL,
including offered captures, decoded representations, selected ticks and
produced-picture ownership. `results.meta.json` binds both artifacts by SHA-256
and records source identity, stdout hashes, commands, the verifier receipt,
selected source profiles and measurement policies. Its applied-patch list is
explicitly empty. Negotiated media attributes omit ICE credentials.

After packing, validate local artifacts against their declared attribution
inputs with:

```sh
python3 crates/pulsebeam-transport/experiments/results.py check
```

This command requires locally generated files; it is not a fresh-checkout or
fast-QA prerequisite. Do not edit generated output by hand or relabel old
measurements after runtime changes. The clean Git revision identifies the full
measured source; named attribution hashes cover the experiment policies,
lockfiles and selected-profile manifests, not every transitive dependency.
Vanilla native qualification does not prove parity with every browser/version.

## Retained proof and cost

The serial experiment owner retains whole-picture retry/retirement, encoded
ordering/dependency, shaped-link, protocol-budget and charged-ciphertext-drop
controls, plus the separate full-native codec sanity control. The native
three-RID capability proof uses a 2Mbit/s startup solely to establish all three
rungs, real PLI/keyframes, aggregate capacity response and SCReAM-positive/GCC-
negative evidence. It is not a comparison row or the runtime startup setting.
Tiny generated scenes and old fixture drivers compile only for these controls.

Corpus codec controls validate decoded scope/capture identity, timestamps,
common-view image calibration and non-epoch resumption keyframes. Sparse real
codec observations retain the full offered denominator and distinguish held
quality from decoded-only quality. Projection matches an independent scalar
box-floor oracle for every rung pair. Projection and repeated reference-error
calculations are cached without dropping measurement coverage.

The fast owner uses bounded correctness fixtures, not the 78-run research
matrix. No browser benchmark is added. Run the full study explicitly when
research results are needed; packing and checking local output reuse those
measurements rather than repeating the matrix. Keep test-child execution costs
separate from build/cache time when evaluating changes to these controls.
