# `pulsebeam-proto`

Protobuf wire definitions and generated Rust types for reliable signaling over
PulseBeam data channels. The crate also owns reserved RTP extension identifiers
and stable signaling topic names.

Edit the files in `proto/`, not generated output in `OUT_DIR`. Wire changes
must account for every producer and consumer before switching endpoints.

`state` defines typed desired/confirmed resource records, prepared transport
facts and capabilities. `state::codec` accepts raw protobuf or one independent
raw LZ4 block with an encoding byte and little-endian decoded-length prefix.
It enforces 4,096 decoded bytes, 4,101 framed bytes and at most 16 records,
with structural preflight before generated decoding and exact input/output
consumption. Callers select client or server direction explicitly.
`state::validation::validate_spec` checks resource configuration separately:
a valid desired envelope with an invalid configuration requires a scoped
rejection, not a malformed-frame failure. Connection owners enforce revision
history, immutable identity, capabilities and authorization.

The optional `json` feature adds generated Serde support for HTTP bootstrap
`RtpCapabilities`, its referenced types, and `DataDelivery` enum-name lists.
Fields serialize with their protobuf snake_case names; optional zero/false
values retain presence. This feature requires `std`; default builds retain the
`no_std` surface. The Bazel `pulsebeam-proto-json` target selects this feature.
The owner unit suite checks both configurations using the existing capability
fixture. JSON parsing does not replace capability validation or the HTTP owner's
body limits and whole-document duplicate-key enforcement. There is no JSON
StateBatch codec.

Native signaling uses only StateBatch framing. There is no Catalog/Intent/Mapping
schema, legacy codec export or fallback decoder. The signaling data-channel label
is unchanged; it does not select a different wire protocol.

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

StateBatch evaluates each resource independently. An unsupported configuration
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
