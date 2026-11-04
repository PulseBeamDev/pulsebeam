# `pulsebeam-proto`

Protobuf wire definitions and generated Rust types for reliable signaling over
PulseBeam data channels. The crate also owns reserved RTP extension identifiers
and stable signaling topic names.

Edit the files in `proto/`, not generated output in `OUT_DIR`. Wire changes
must account for every producer and consumer before switching endpoints.

HTTP bootstrap selects `pulsebeam.media.v1` once. Each binary message on
negotiated channel 0 is one raw protobuf `ClientStateBatch` or `ServerStateBatch`,
with no encoding prefix or compression. `state::codec` enforces 1..4,096 bytes
and at most 16 resource records, with structural preflight before generated
decoding. Use `encode_client`/`decode_client` for owned desired proposals and
execution reports, and `encode_server`/`decode_server` for authoritative observations. Their record
types share resource values, not an envelope that permits either direction.
`state::validation::validate_spec` checks resource configuration separately:
a valid desired envelope with an invalid configuration requires a scoped
rejection, not a malformed-frame failure. Connection owners enforce revision
history, immutable identity, capabilities and authorization.

## Extensible RTP descriptions

`CodecFormat` carries a lowercase MIME type, clock rate, optional channel count,
and named FMTP parameters. Feedback and header extensions use their transport
names and URIs, not a closed wire enum. Descriptions retain unknown parameters
and formats; validation does not grant transport or decoder support.

Use the explicitly advertised capability intersection and actual engine
observations before installing media. Unsupported resources fail locally without
changing accepted resources. Omission keeps its defined meaning, and established
bindings do not change because unrelated capabilities change.

The optional `json` feature adds generated Serde support for HTTP bootstrap
`RtpCapabilities`, its referenced types, and `DataDelivery` enum-name lists.
Fields serialize with their protobuf snake_case names; optional zero/false
values retain presence. Unknown optional JSON fields are skipped; malformed known
fields and duplicate known fields are still rejected. This feature requires `std`;
default builds retain the
`no_std` surface. The Bazel `pulsebeam-proto-json` target selects this feature.
The owner unit suite checks both configurations using the existing capability
fixture. JSON parsing does not replace capability validation or the HTTP owner's
body limits and whole-document duplicate-key enforcement. There is no JSON
state-batch codec.

This pre-release structural cutover retains `pulsebeam.media.v1` and requires
coordinated endpoint updates. Subsequent additive optional descriptions can evolve
without another structural cutover. Unsupported profiles are rejected during HTTP
bootstrap.
Future compression requires an explicitly supported profile, not per-message
negotiation. There is no Catalog/Intent/Mapping
schema, legacy codec export or fallback decoder. The signaling data-channel label
is unchanged; it does not select a different wire protocol.

## Prepared media reception and failure

`SubscriptionSpec.binding` carries the complete RTP or SCTP configuration after
successful local platform preparation. RTP bindings contain the independently
allocated receive identities and negotiated codec/PT, extension and RTCP maps,
never copied publisher demux coordinates. The source descriptor supplies its
admitted codec envelope; HTTP capabilities supply server support.

The server authorizes the exact source and validates its format, binding,
identity conflicts and limits before admitting the configuration. It confirms the
accepted resource and can forward without another installation acknowledgment.
There is no server-authored receive plan or `Preparing` phase. Bindings remain
immutable for their owned lifetime. A new source incarnation requires a fresh
receiver and binding, not silent active-binding retargeting.

Local readiness follows successful platform description completion and current
generation/operation/owner correlation. It does not require packet arrival,
frames, unmuting or exhaustive receiver readback. Application-visible readiness
also requires authoritative server acceptance and the current receiver lifetime.

A later local receiver failure uses `ClientStateRecord.execution.failure`. A
record cannot contain both `desired` and `execution`. Failure reports use the
same connection-wide monotonic input clock, with a revision later than the owned
reference; they do not advance the desired `evaluated_revision` or replace its
accepted specification. A rejected settings proposal cannot hide an independent
execution failure. Failure closes forwarding and demand for that lifetime;
settings or source availability cannot revive it. Withdrawal and physical
cleanup retain the existing authorization, source and retirement fences.

## Enumeration and ordered recovery

The server's `initial_complete` marker means every initially visible key has
been represented. It is not an atomic snapshot, a batch acknowledgment, latest
desired-state convergence or evidence that an undiscovered source has ended.
Apply records before observing the marker.

Native data subscriptions name an exact opaque source and an actual prepared
payload binding. Recovery must preserve still-owned ordered frontiers across
transport replacement, without retaining queued payloads or using empty
discovery to infer retirement. Probes need current owned confirmation and source
facts; an unknown-source timeout or internal failure is not authoritative source
termination.

See [Core ordered recovery](../../agents/pulsebeam-agent-core/README.md#ordered-subscription-recovery)
for frontier ownership. New source IDs start live; explicit unsubscribe and
session close release frontiers. TopicFrame payload and bounded ordered-gap
semantics are unchanged.

## Unsupported receive settings

The server evaluates each resource independently. An unsupported configuration
rejects that resource's evaluated target without replacing its accepted value,
rewriting client desire or rolling back unrelated resources. Rejection does not
settle a newer target and is distinct from malformed-wire or fatal transport
failure.

Stock native and browser audio do not negotiate RTP playout-delay, so use
adaptive audio. The native resource receiver rejects fixed audio with
`FailureCode::Unsupported` before changing accepted receive state. A local
minimum-delay setter does not implement the full min/max interval.

Run `./bazel test //crates/pulsebeam-proto:unit_tests` for focused verification,
then the root `./bazel test //:check` and `./bazel test //:test` gates.
