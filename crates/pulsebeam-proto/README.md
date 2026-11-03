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
body limits and whole-document duplicate-key enforcement. No JSON StateBatch
codec or live HTTP endpoint switch is included.

Existing Catalog/Intent/Mapping endpoints use `signaling_v1` and `codec`, whose
independent raw LZ4 envelopes have 32,768-byte decoded and 32,912-byte wire
limits. These bytes are not interchangeable with StateBatch frames.

## Ordered subscription recovery

An open ordered subscriber sends `SubscriptionSeed` on reliable signaling.
The controller resolves its initial matching publications, then transfers the
completion marker through the same ordered shard-update lane as their catalog
projection. Signaling commits the covering nonzero Catalog revision before
sending `SubscriptionSeedComplete`. Output backpressure cannot skip that Catalog
or let completion overtake it.

Core preserves the last completed publication frontiers during incomplete
replacement seeds, but not queued payloads. It gates recovery probes and ordered
delivery until matching completion, then removes genuinely absent publications.
New publication IDs start live. Unsubscribe and session close release frontiers;
replaced transports fence tokens and retain only still-owned frontiers. Unknown
tokens and zero/future covering revisions are protocol errors. A known retired
token may complete in flight, but cannot recreate its subscription or frontiers. A connected seed
that remains incomplete for 30 seconds fails connection recovery and retries,
rather than treating an empty bootstrap Catalog as retirement.

## Unsupported receive policies

`UNSUPPORTED_INTENT` non-fatally rejects one candidate revision before any send
bindings or receive state commit. Its Error has `fatal=false` and the candidate
`intent_revision`; accepted Intent and Mapping remain unchanged. Clients surface
the existing server-error notification without replacing the connection or
rewriting desired state. It is distinct from authorization rejection and fatal
protocol errors.

Fixed audio playout requires the RTP playout-delay capability on every negotiated
audio receiver. Stock native and browser WebRTC audio do not negotiate it, so
use adaptive audio. A local minimum-delay setter does not implement the full
min/max interval. Fixed video support is unchanged. The capability check does not
advertise unsupported SDP or bypass RTP extension validation.

Run `./bazel test //crates/pulsebeam-proto:unit_tests` for focused verification,
then the root `./bazel test //:check` and `./bazel test //:test` gates.
