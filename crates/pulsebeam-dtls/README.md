# PulseBeam DTLS

Native, caller-driven DTLS using **dimpl 0.9.0 and AWS-LC**. The default
association provides pinned DTLS 1.3 datagrams. An explicit DTLS-SRTP profile
supports WebRTC protection through the same authenticated engine. No sockets,
runtime, CID migration, provider selection or shared-PSK API are included.

## Authenticate a peer

Generate a `LocalIdentity`, publish its `fingerprint()` through your authenticated
control plane, and obtain the expected peer's `Fingerprint` through that same
trusted provisioning. Bind each pin to the intended node and role. Pins are
public but their distribution must be authenticated. No public CA is required.

```rust
use std::time::Instant;
use pulsebeam_dtls::{Association, Error, Fingerprint, LocalIdentity, Role};

// Call after authenticated provisioning supplies the expected peer pin.
fn connect(
    local: &LocalIdentity,
    expected_peer: Fingerprint,
    mtu: usize,
    now: Instant,
) -> Result<Association, Error> {
    Association::new(Role::Client, local, expected_peer, mtu, now)
}
```

Both roles check the provisioned SHA-256 fingerprint of the peer's entire DER
leaf certificate. `Output::Connected` and application sends/receives are gated
on both handshake completion and the matching pin. A mismatch destroys the
association. Reuse the local identity to keep its pin stable across associations;
traffic state is independent. Identity generation uses AWS-LC-backed ECDSA P-256.
Generated identities are in-memory. `LocalIdentity::from_der` also imports a
caller-owned DER certificate and matching P-256 PKCS#8 private key. The caller
owns secure persistence and must supply a matching pair; the wrapper does not
store keys externally or expose a private-key accessor.

## Select the DTLS-SRTP profile

`Association::new` remains DTLS 1.3-only and never exports SRTP keys.
`Association::dtls_srtp` selects `SrtpVersion::Auto`, `Dtls12` or `Dtls13` for
WebRTC. Auto prefers DTLS 1.3 with DTLS 1.2 compatibility. Its
`Output::KeyingMaterial(material, profile)` is released only after both the
handshake and expected certificate pin are verified. It is a purpose-specific
DTLS-SRTP exporter, not a general TLS exporter API.

The [transport DTLS-SRTP profile](../pulsebeam-transport/README.md#select-a-transport-profile)
owns ICE-selected paths and SRTP/SRTCP protection. This crate owns neither ICE,
SDP negotiation, SRTP packet state nor an SCTP association.

## Drive datagrams and time

After every operation, drain `poll_output(buffer)` until `Output::Timeout(at)`:

- `Datagram(bytes)`: send to the association's selected peer path.
- `ApplicationData(bytes)`: deliver one authenticated application datagram.
- `Connected`: the authenticated association is ready for application traffic.
- `KeyingMaterial(material, profile)`: install authenticated SRTP keys; emitted
  only by the explicit DTLS-SRTP profile. Never log the material.
- `Progress`: continue polling; there is no application-visible payload.
- `BufferTooSmall { needed }`: enlarge the caller buffer and retry polling.
- `Timeout(at)`: schedule the next wake-up.
- `Closed`: stop polling and discard the association.

For a UDP arrival, first call `handle_timeout(now)` and drain output, then call
`handle_packet(datagram)` and drain again. For a timer, call
`handle_timeout(now)` and drain. Time must be monotonic. Application sends use
`send_application_data(payload)` followed by draining. Do not interleave engine
operations before draining to `Timeout`; dimpl requires that boundary for
consistent state. Borrowed output is valid only until its caller buffer is reused.
During the handshake, that buffer must fit the peer's DER certificate: dimpl
0.9.0 asserts on an undersized certificate-output buffer rather than returning
`BufferTooSmall`. This can abort the process before pin verification. Other
buffered packet/application outputs can be resized and retried.

Supply only datagrams from the selected path. Reconnect with a fresh association
after an address change. The wrapper does not discover, validate or migrate UDP
paths. The higher layer owns path MTU selection, application payload sizing and
any application fragmentation. The constructor forwards its caller-provided
`mtu` to dimpl for handshake fragmentation and record packing. This is not an
application-size limit: dimpl does not split application data to fit the MTU and
can emit a larger datagram if given a larger application record. Size payloads
appropriately before sending. The wrapper adds no payload cap or size-based
inbound discard; dimpl's protocol errors propagate normally. Empty application
datagrams are supported. Queued receive/transmit records are bounded by dimpl's
configured queues; this is not a total pre-authentication memory qualification.

Protocol errors are fatal and discard the engine. Pre-authentication send and
backwards-time errors leave it intact. `close()` immediately discards
local state; it does not guarantee peer notification. `notify_close()` instead
queues close-notify for the caller to drain before discarding state; this is not
a delivery guarantee. There is no raw DTLS traffic-key API. Dimpl manages retransmissions, replay
handling and automatic KeyUpdate using its default key-use limit; manual
KeyUpdate is not exposed. This crate is not a production-security or high-rate
performance qualification of the upstream engine.

## Contributor checks

From the repository root:

```sh
./bazel build //crates/pulsebeam-dtls:pulsebeam-dtls
./bazel test //crates/pulsebeam-dtls:unit_tests //crates/pulsebeam-dtls:format //crates/pulsebeam-dtls:clippy
```
