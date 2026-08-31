# pulsebeam-rtc

`pulsebeam-rtc` is the WebRTC boundary for PulseBeam's SFU. One shard owns one
`RtcPeer`. The peer is `Send`, so its owning task may move between scheduler
threads, but it is never shared or polled concurrently.

PulseBeam interacts with RTC through four operations:

1. Feed an `IngressDatagram`, drive `handle_timeout`, and drain `RtcEvent`.
2. Select a source/layer and pass its `MediaPacket` plus `MediaRewrite` to an
   `EgressSlot`.
3. Set the allocator's aggregate desired and current media bitrates.
4. Drain `Transmit` values and complete each `DepartureReceipt` after the
   socket send succeeds or fails.

PulseBeam owns selection and the logical viewer timeline. It decides which
packet fills each logical sequence/timestamp position, including out-of-order
switch completion. It does not encode final RTP headers or request individual
RTX, padding, or probe packets.

`pulsebeam-rtc` owns negotiation, authenticated packet parsing, final sequence
mapping, RTP/RTCP, SRTP/SRTCP, SCTP, TWCC, downstream GCC, pacing, RTX history,
padding, SSRC 0 fallback, and transport deadlines. It publishes one
`BweCapacity`; PulseBeam does not filter it again.

`MediaPacket` parses authenticated packet components lazily and caches every
result. Same-shard forwarding borrows its payload. A cross-shard or cross-node
hop explicitly creates one `TransitMediaPacket`; no shared packet ownership is
hidden in the facade.

## Quality gates

- `cargo test -p pulsebeam-rtc` runs facade, protocol, GCC, pacer, RTX, padding,
  SSRC 0, and upstream-str0m compatibility contracts.
- `make test-sim` runs the deterministic real-decoder SFU matrix.
- `make test-sim-soak` runs the explicit two-minute decoded-media soak.
- `make test-browser` runs one production-path contract in Chromium, Firefox,
  and Linux WebKit.
- `make check-agent-str0m` proves `pulsebeam-agent` resolves official upstream
  str0m rather than the PulseBeam fork.

The complete ownership and acceptance specification is
[`plans/pulsebeam-rtc-quality/spec.md`](../plans/pulsebeam-rtc-quality/spec.md).
