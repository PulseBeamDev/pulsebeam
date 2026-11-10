# pulsebeam-transport

Caller-driven secure transport for native datagram services and WebRTC. The datagram service has swappable DTLS 1.3 and native Noq QUIC DATAGRAM backends. The host owns UDP, receive timestamps, time and scheduling. Transport owns protection, attempt identity, bounded feedback history, SCReAM and outer pacing. Link owns routing, application encoding, caller queues and local fanout.

This implements the native datagram service from [RFC #290](https://github.com/PulseBeamDev/pulsebeam/issues/290). Noq is an unpatched, pinned `noq-proto` dependency. This is raw QUIC with a PulseBeam ALPN, not HTTP/3 or browser WebTransport.

## Select a transport profile

- The default `datagram` feature exposes the native application service described
  below. It owns SCReAM, outer pacing and attempt feedback for both backends.
- The opt-in `dtls-srtp` feature exposes `dtls_srtp::Transport`: ICE-selected
  UDP/passive-TCP paths, authenticated DTLS, SRTP/SRTCP replay/protection and
  bounded prepared outputs. Use `default-features = false` when only this profile
  is needed. It does not pull in Noq or own SDP, SCTP associations, RTP routing,
  RTCP feedback policy, congestion control or pacing. `pulsebeam-rtc` supplies
  negotiated facts and retains those connection-level responsibilities.

Both DTLS profiles use `pulsebeam-dtls::Association`. DTLS-SRTP prepares output
only after caller admission succeeds, and returns exact protected bytes and
identity/path metadata for the RTC send commit. It does not commit RTC flight
history or decide when media should be sent. The datagram service's socket
acceptance contract below is separate from RTC's commit-on-return contract.

## Drive a provisioned association

1. Construct `Peer` for the preserved DTLS profile, or `QuicPeer` with `QuicCredential` and `QuicTuning::default()`. Both require a fixed endpoint, a local credential, the intended peer's trusted certificate fingerprint, role and feedback class. Pass either to `Transport::new`. Sending never provisions a peer. Link's `open_network` and the native SFU's trusted `open_peer` accept either profile too.
2. Call `start(Time, Instant)` with observations from the same host clock. Subsequently inject monotonic nanoseconds through `poll`. Both secure engines use caller-driven time; the Noq adapter also supplies injected rustls and token-validation clocks.
3. Drive `poll(now, work_budget, send, receive)` and schedule `next_deadline()`. `needs_progress()` requires an immediate bounded continuation. Pass incoming UDP to `receive(source, bytes, observed)` only when `input_ready()` is true; `Busy` leaves input ownership with the host. Retain input in bounded per-peer queues, without blocking unrelated peers. A zero work budget advances the time fence without processing protocol work.
4. Keep the main application queue outside transport. Set demand with `set_demand`, then use non-reserving `admission()` before expensive encoding. `try_submit(context, payload, expires_at)` borrows input and copies at most one admissible frame. Rejection leaves ownership with the caller. `payload_capacity()` includes the backend's negotiated usable limit, not just the UDP limit.
5. The `send` callback must report actual nonblocking socket submission. Only `Accepted { bytes }` matching the complete UDP payload commits outer flight accounting. DTLS discards rejected or short outputs. Noq retains one serialized UDP output and retries after a bounded delay because the library has already advanced QUIC packet/recovery state. Accepted outputs are never replayed. A batching host must report only its accepted prefix.
6. Consume authenticated datagrams in the receive callback. Receipt is recorded before invoking it, even when the application drops the data. Callbacks get the original injected monotonic/wall observations; absent wall observations stay `None`. Close explicitly or drop the engine when the provisioning owner retires it. Reconnection/backend changes create a new association and must fence old host input and application bindings.

`Time` and `expires_at` are local monotonic nanoseconds. Never serialize an absolute local expiry as a remote deadline. Poll with the current host time before admission. Both profiles check expiry before admission and immediately before socket submission, including retained ciphertext. Neither promises remote delivery before expiry or revocation of already submitted traffic.

## Noq profile and limits

PulseBeam decides application admission and send rate. Noq supplies standard secure QUIC DATAGRAM delivery with deliberately permissive, finite inner ceilings. It still has ACKs, RTT/loss detection, PTO and pacing. Lost application datagrams are not retransmitted.

| Area | Configuration |
| --- | --- |
| Application protocol | `pulsebeam-datagram/1` ALPN; zero incoming bidirectional/unidirectional application streams; no application stream opens. |
| Authentication | Mutual exact SHA-256 certificate pins and verified TLS 1.3 CertificateVerify signatures. As with DTLS, trusted provisioning establishes identity, not DNS/CA trust or wall-clock certificate validity. |
| Early data | Application 0-RTT, TLS resumption and server tickets disabled. |
| Inner congestion window | Non-adapting 32 MiB default safety ceiling, twice the outer ledger's 16 MiB funding budget. Loss response stays with the outer controller. |
| Inner pacing | Explicit 1,000,000,000-byte/s default ceiling and 1200-byte controller send quantum. This is headroom for the tested workload, not a pacing-disable switch or an assumed path capacity. |
| Queues | One application/feedback frame across the adapter; 1200-byte Noq send-buffer budget with `drop = false`; 32-MTU receive-buffer budget; at most one retained UDP output. No silent replacement of older admitted application work. |
| Path | Fixed 1200-byte UDP payload ceiling; no PMTU discovery, migration, multipath, NAT traversal or observed-address extensions; QUIC bit greasing and spin bit disabled. |
| Recovery | Normal QUIC ACK/loss/PTO behavior. No requested ACK-frequency tuning. |
| Work | One UDP datagram per Noq transmit poll; segmentation offload disabled. Handshake, endpoint and control work consume the caller's poll budget. One provisioned association per explicit Noq endpoint; additional Initials are ignored after association creation. |
| Outer feedback failure | Stop application admission and retire the association after two seconds without newly acknowledged outer traffic while feedback is outstanding. QUIC ACKs alone cannot keep it authorized. Idle associations with no outstanding application feedback do not trip this guard. |

`QuicTuning` exposes finite inner window and pacing ceilings. Lowering them can make Noq binding. Do not increase them as a substitute for a working outer controller. `NetworkClass` changes outer feedback cadence only, not an assumed link capacity or RTT.

### Weaker cancellation and send-timing boundaries

Noq cannot remove an individual already queued DATAGRAM through its public API. An expired frame can remain there until QUIC makes progress; the adapter drops its eventual serialized output before UDP submission. This does not undo Noq's internal packet/loss accounting. `queue_cancellation_supported()` reports false; setting `require_queue_cancellation` rejects construction with `UnsupportedProfile`. Strict internal-queue cancellation profiles are unsupported without upstream support.

Likewise, Noq accounts transmission at `poll_transmit`, before kernel submission. The retained output makes nonblocking socket retry safe, and the outer ledger commits only actual callback acceptance, but exact kernel-send timestamps for QUIC's internal recovery are unavailable. A prolonged socket stall can therefore affect inner recovery statistics. Do not present them as authoritative outer congestion feedback.

### Observe interference

`Health::staging` reports the one frame's plaintext size and age. `pending_udp_bytes` distinguishes socket retry retention. Optional `quic` statistics expose actual QUIC ACKs/loss, current inner window, queued payload bytes and polls that found queued application work without an output. These do not fabricate a separate pacing-versus-window delay attribution: Noq does not expose that split. Link and the native host's health snapshots preserve these observations and the first terminal reason.

The matched low-delay test uses the same four-Mbit/s offered payload trace for both backends with one-millisecond host turns. It checks bounded staging and outer accounting, and no Noq inner stalls in that envelope. It is not an Internet congestion-quality, maximum-throughput, CPU or allocation benchmark. Higher rates, large RTTs and sustained impairment require further workload-specific qualification before claiming negligible inner interference there.

## Shared UDP ports

The host owns the UDP socket and explicit peer/association dispatch. `classify` implements RFC 9443 first-byte classification, with QUIC bit greasing disabled. TURN ChannelData wins the overlapping byte range only for an explicitly known TURN source. Multiple DTLS usages still need tuple/association context. Classification never authenticates or provisions a peer.

The native smoke test runs real Noq, DTLS Link and the production WebRTC connection on one externally owned server UDP socket. It verifies WebRTC SRTP and DTLS progress while one QUIC peer's output is rejected, followed by successful QUIC retry and outer feedback settlement. It does not consolidate the SFU node's existing separately configured WebRTC and Link listener sockets or provide a TURN client.

## Preserved application framing

The DTLS profile remains certificate-pinned DTLS 1.3 without PSK, CID, migration or application 0-RTT. Its 1200-byte protected UDP limit and conservative 64-byte security allowance retain a **1126-byte application payload limit**, plus the opaque four-byte context. Noq exposes the smaller of its usable negotiated capacity and this service limit. There is no fragmentation, reassembly, application retransmission or ordering service.

The authenticated data plaintext remains:

```text
version=1 | kind=0 | application context[4] | attempt u32 BE | payload
```

Link places its receiver-issued route key in that context; transport neither interprets it nor depends on Link. Feedback keeps its bounded `version=1, kind=1` base/generation/status encoding. Attempt unwrapping, association fencing and feedback validation stay transport-owned. Byte-exact fixtures pin the old framing, while Link tests retain routing, fanout, local-overload, reconnect and sequence-wrap coverage for both backends. DTLS key-update qualification remains backend-specific.

A browser WebTransport adapter would expose only the service-facing datagram boundary. Browser congestion control and hidden queues remain authoritative. That adapter and reliable control services are outside this native implementation.

## Validate changes

Run owner checks from the repository root:

```sh
./bazel test //crates/pulsebeam-transport:unit_tests //crates/pulsebeam-transport:shared_port_test //crates/pulsebeam-transport:format //crates/pulsebeam-transport:clippy //crates/pulsebeam-link:unit_tests
```

Unit tests cover both features, including the moved ICE, SRTP/replay and
prepared-output tests. The RTC owner retains the real history-admission proof
and native media/channel interoperability tests. Bazel's `dtls_srtp` library
builds the RTC-only profile without the datagram controller or Noq dependencies.

The crate and native smoke are registered in `//:fast`; formatting and lint checks are in `//:check`. Deterministic tests exercise production engines with injected time. The single loopback smoke covers actual UDP/demux behavior without browser automation or sleeps. See the [contributor testing contract](../../docs/build.md#testing-contract).
