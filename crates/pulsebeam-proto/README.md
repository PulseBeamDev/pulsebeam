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

## Media receive plans and execution

An RTP `SubscriptionSpec` carries `rtp_offer` from the endpoint's applied local
receive offer and leaves `binding` absent. The offer describes its actual MID,
codec/PT possibilities, header-extension mappings and RTCP modes without choosing
receive SSRCs or CNAME. Data subscriptions still carry their prepared SCTP binding.
HTTP bootstrap capabilities alone are not endpoint receive capabilities.

The server selects a source-compatible intersection, reserves fresh nonzero
output SSRCs and publishes the authoritative `LiveResource.binding` with
`Preparing` execution. That binding is immutable for the owned lifetime. A new
source incarnation requires a fresh receiver and plan, not an active-binding
retarget. Unrelated capability or settings changes do not reassign its identities.

After actual platform application and readback, the endpoint sends a separate
`ClientStateRecord.execution`: either the observed installed binding or a scoped
failure. A record cannot contain both `desired` and `execution`. Reports use the
same connection-wide monotonic input clock, with a revision later than the owned
reference; they do not advance the resource's desired `evaluated_revision` or
replace its accepted specification. A rejected settings proposal therefore cannot
hide an independent execution failure.

Forwarding and installed execution require matching endpoint evidence as well as
the existing authorization, source and retirement gates. Endpoint readiness also
requires its own matching local facts, not server status alone. Installation does
not wait for RTP packets or statistics; those cannot arrive reliably before the
forwarding gate opens. Malformed reports, scoped failures and superseded lifetimes
remain distinct from a successful installation.

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
