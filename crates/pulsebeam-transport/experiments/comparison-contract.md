# RTC uplink comparison contract

This is the replacement experiment's implementation contract, not a description
of the existing CSV runner or evidence that the replacement has passed. It
compares sender/uplink behavior with a shared experimental receiver. It does not
compare complete native playout, SFU downlink savings, browsers or ECN/L4S.

## Capture and observation records

The offered ledger is constructed before driving any arm. One record represents
one source capture, not one encoded representation. All arms consume the same
ledger and run-relative clock. Startup begins at capture zero, after connection
provisioning; no readiness predicate removes offered captures.

| Record | Required fields and meaning |
| --- | --- |
| Capture key | `source`, `lifetime`, `epoch`, `capture`. Lifetime fences replacement sources; epoch fences resets within a lifetime. Capture is a strictly increasing run-local sequence number. |
| Offered capture | Capture key, `capture_ns`, canonical reference key. Written even if no encoder produces output. |
| Representation key | Capture key plus negotiated `layer` (`q`, `h`, `f`). Alternative layers do not increase the offered denominator. |
| Produced representation | Representation key, actual codec bytes, dimensions, keyframe/dependency facts where observable, provenance kind and evidence. |
| Decoder-ready observation | Representation key, `ready_ns`, observation sequence, actual decoded dimensions and pixels/reference error. Encoded submission, packet assembly and decoder admission are not decoder-ready observations. |
| Terminal lifecycle | Representation key, ownership stage, snapshot time, exact observed disposition or `unknown` with reason. Kept independently of presentation. |

All time values are integer nanoseconds relative to a recorded run origin.
Capture time is the injected source clock, not callback arrival time. `ready_ns`
is the host observation when an actual decoded output becomes available. Record
transport/codec clock origins and RTP base separately; unwrap RTP timestamps
within a lifetime before mapping to the offered ledger. Reject unmatched,
ambiguous, future-capture and cross-lifetime observations rather than inventing
capture times. Equal-time observations have a stable sequence tie-break.

Provenance kinds distinguish fixed encoded replay, controlled live source input,
observed encoded output and decoded identity. Caller assignment or a successful
push alone does not establish output origin. Binding MID/RID/SSRC to a source is
necessary but does not prove a particular capture was encoded. Require actual
output identity (codec timing plus a validated content identity or an actual
encoder output association). Record uncertainty explicitly. An unknown identity
cannot earn decoded or presented coverage.

Source resets retain earlier offered captures and increment the epoch or
lifetime. Old output cannot be relabelled as new output. A configured reset
invalidates the held image until an identified new-epoch output is ready. The
comparison does not infer a source reset from network reordering.

## Presentation policy `offered-timeline-v1`

Use a bounded, pure event reduction, independent of transport/native types.
Define presentation ticks from zero through the exclusive capture horizon `H`;
the tick interval and freeze threshold are recorded policy inputs. Each tick
represents `[tick, min(tick + interval, H))`, so startup and the final tail are
included. Drain observations may establish eventual delivery but cannot rewrite
past presentation or extend the offered horizon.

At a tick, consider only identified observations ready at or before that tick.
Select the newest capture in the current source lifetime/epoch, breaking ties by
higher spatial layer and then stable observation sequence. Never rewind to an
older capture or downgrade an already held representation of the same capture.
An upgrade is a new representation, not a new capture. Without a usable image,
charge blank duration. Otherwise charge presented duration and displayed-frame
age `tick - capture_ns`. Charge held duration when the same capture remains
visible across successive tick intervals. A freeze is continuous held time that
reaches the configured threshold; layer upgrades do not end a capture freeze.
Report threshold, total freeze duration, longest freeze and freeze count.

Per offered capture, derive timely/late/missing/unknown decode status from the
first validated decoder-ready observation and the common recorded deadline.
Missing means no such observation by the bounded observation cutoff. Unknown is
reserved for invalid/incomplete measurement evidence, not a favorable substitute
for missing. Repeat outputs cannot increase capture coverage; reordered old
outputs cannot repair past ticks. Report identity failures separately and mark
the run unqualified when required evidence is unavailable.

Report decoded-only image error separately from duration-weighted presented
image error against the canonical reference at each presentation tick. Held
images are compared with the current offered source, not with their own old
reference. Do not assign a fictional PSNR to blank time: report blank duration
alongside quality, with its conditional denominator. Preserve squared error and
pixel/sample counts before calculating aggregate PSNR. Perfect matches remain
explicit, not arbitrary finite values. Different layer dimensions must use the
same documented common-view transform and canonical source; existing independent
layer references do not automatically meet this requirement.

Work and retained memory must be bounded by offered captures, produced
representations, observations and presentation ticks. Do not iterate per
nanosecond or expand an unbounded drain.

## Ownership and terminal accounting

For custom pictures, record produced units, total fragments, accepted fragments
and queued fragments. At the common drain cutoff, explicitly retire caller-owned
unadmitted pictures and partially admitted picture tails as terminal undelivered
work, preserving accepted-prefix counts. Full admission is not delivery. Keep
incomplete receiver assemblies, dependency discards and decoder rejection
separate. No controller-specific deadline, expiry or retransmission advantage is
introduced by measurement.

Native encoded-input pending counts remain snapshot observations with unknown
age/ownership disposition unless the actual API establishes more. Do not divide
an aggregate pending count among invented capture identities or count it as
successful delivery. Native raw encoder omissions are not classified into
pause/backpressure/drop reasons without direct observations. Lifecycle unknowns
and presentation missingness can coexist without contradiction.

## Versioned result set `rtc-uplink-v1`

The cutover replaces the old CSVs and metadata together. Use one JSON result
set, with an explicit schema version and the following logical sections. Do not
interpret historical CSVs as this schema.

- **Manifest:** actual candidate commit and dirty fingerprint; exact command;
  native source/profile and Rust controller identities; workload kind; corpus
  manifest/generator/asset hashes; selected source/lifetime/epochs/cadence;
  common-reference transform; codec settings; timebase/origins; horizon/drain;
  presentation/deadline/freeze policy; scenario/model version and seeds.
- **Per-run observations:** arm/backend/inner and outer controller, actual
  negotiation/layer/controller-selection evidence, offered ledger, produced
  representations, decoder-ready outputs and terminal lifecycle observations.
  Unavailable fields are explicit nulls with reasons, never invented zeros.
- **Per-run summary:** all-offered capture coverage; startup blank, held, blank,
  freezes, displayed age; timely/late/missing/unknown counts; decoded-only and
  presented quality with denominators; actual attempted/accepted/dropped wire
  bytes by direction; source/encode/decode/disposition counters and boundedness
  failures. Preserve individual seeds, not only pooled percentages.

Six adaptive arms are native SCReAM, native GCC, custom DTLS/UDP, and custom QUIC
OuterControlled/CUBIC/BBR3. Each has one aggregate path budget funding all active
rungs before application selection. Native uses receiver-only production
`RtcConnection`, negotiated TWCC, and genuine RTCP feedback/repair. The separate
full-native single-stream codec sanity control is not a seventh comparison row.
Fixed replay and live source adaptation are separate provenance claims.

## Cutover map and retained proof

Paths below are relative to this directory unless otherwise noted. Existing
proof is in `//crates/pulsebeam-transport:experiment_controls`; these mappings are
migration obligations, not new passing claims.

| Current file/symbol or surface | Cutover | Unique claim / retained owner |
| --- | --- | --- |
| `webtransport_poc.rs::{Scenario, Link}` | Keep and reuse controlled link | FIFO/direction independence, overflow without service debt, rate-boundary integration and measurement handoff: first four tests in `controls.rs`. |
| `webtransport_poc.rs::Session` | Keep production transport driving | Correct UDP/QUIC identity and accepted-send behavior; shared by custom workload. |
| `webtransport_poc.rs::{main, diagnostic_main}` | Replace dispatch with replacement workload only; delete `--diagnostic`, `--audit`, `--drop-protected`, truncated drain, TTL and poll-budget parser branches and diagnostic stdout CSV | No default-ranking claim depends on this CLI. |
| `webtransport_poc.rs::{Audit, quic_run, Metrics, frame, frame_id, packets}` | Remove CLI/report behavior; isolate only the minimal fixture/driver still needed by correctness tests in `controls.rs` | `coarse_host_turns_and_one_unit_polls_preserve_feedback_progress` and `dropping_charged_ciphertext_fabricates_loss_on_a_lossless_high_rtt_path`. Production poll/retained-output contracts are not deleted. |
| `webtransport_poc.rs::{Native, native_run}` | Preserve only shared native/session plumbing or migrate to receiver-only topology; remove padded native ranking branch | Full-native single-stream codec sanity remains separate. `fixture_padding_preserves_independently_decoded_pixels` may retain a tiny test-only fixture, not a ranking mode. |
| `video.rs::{pixels, picture_id, Trace}` | Replace headline generated 320x180/25fps scene with corpus-derived fixed/live inputs | Keep tiny generated fixture only for `actual_codec_preserves_picture_identity_and_timestamp_without_ancillary_data` and distinct codec controls until compatible migration. |
| `video.rs::{VideoMetrics, print, run, Arm}` | Replace decoded-only CSV/ranking and mixed fixed/bare matrix | New ledger/presentation/results contract above. Remove `--diagnostic-controls`, bare branches and fixed ranking rows; migrate unique fixed proof to tests. |
| `video.rs::{native_video_mode, native_video_controller}` | Replace native headline full-receiver pipeline with simultaneous q/h/f sender to receiver-only RTC | Positive SCReAM/negative GCC and genuine TWCC response must be proved in the actual three-RID topology before workload expansion. Existing single-stream proof is insufficient. |
| `video.rs::{datagram_video_mode, fragments}` and `encoder.rs::SourceEncoder` | Migrate to common corpus capture identity and aggregate layer funding | `native_udp_reference_and_quic_control_decode_real_media` preserves native fixed decode, custom UDP/QUIC fixed decode and guided-source accounting; it is not adaptive quality proof. |
| `media_queue.rs::PictureQueue` | Preserve whole-picture retention; add explicit terminal accepted-prefix/queued-tail disposition | `backpressure_retains_the_rest_of_an_encoded_picture_without_caller_expiry`. |
| `video.rs` receiver ordering/dependency tests | Keep or migrate assertions to new common receiver | `completed_old_picture_is_never_submitted_after_a_newer_picture` and `unencoded_capture_gaps_are_not_missing_encoded_references`. |
| `video.rs::decode_gap_includes_startup_and_missing_tail_but_not_drain` | Replace callback-gap headline with presentation proof; retain its startup/tail/drain regression | Pure measurement tests must distinguish callback gaps from actual held/blank presentation. |
| `native_profile.rs` and qualified controller owners | Keep source/controller selection and independent alignment proof | Native mode:always plus observed SCReAM log; GCC disabled and no SCReAM log. No new controller tuning. |
| `results.before-validity-audit.csv`, `results.before-comparison-fairness.csv`, `results.before-native-scream.csv`, `results.before-native-scream.dropper-diagnostic.csv` | Delete during evidence cutover | Checkout consumer inspection found only experiment README references, no executable reader. History retains artifacts. |
| `results.csv`, `results.long-rtt.csv`, `results.meta.json`, experiment README ranking tables/links | Keep generated results local and ignored; retain reproduction and interpretation guidance only | Pack scalar rows, traces and source provenance together when generating local evidence. Do not commit generated snapshots or relabel old measurements under a new native baseline. |
| `../README.md` experiment baseline assertions | Reconcile with actual pinned/qualified native source, remove stale rankings | No root README change. |

## Corpus and API migration boundary

`pulsebeam-testdata::QualityVideoIdentity` currently identifies source/layer/
epoch/frame; `QualityCorpusVideo::frame` supplies capture timestamps and
`reference_frame` supplies same-layer decoded pixels. The existing six-second,
30fps synthetic corpus has two 90-frame epochs. Keep its existing consumers
compatible; introduce canonical source truth through its generator/manifest,
not by editing generated assets. Independent post-scale layer labels currently
prevent assuming cross-layer pixel equivalence.

`ProductionSession::new_simulcast` and `EncodedVideoSource::push_simulcast`
provide genuine ordered three-rung encoded input, not live allocation proof or
capture identity by themselves. Source rate/keyframe feedback is observed via
native APIs; keyframe feedback polling does not synthesize an Encode invocation.
Use real raw source/allocation APIs for live adaptation and actual output
observations for identity. The common receiver uses inbound MID/RID and
`request_keyframe(ssrc)` with no outbound media registered. No production SFU
custom-ingress feature is part of this migration.
