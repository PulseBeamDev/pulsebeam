# Run an encoded-media worker

Use the Rust `encoded_worker` example to record a pinned Opus, H.264, VP8, VP9, or AV1 publication, or to process fresh complete encoded units without decoding. Build from this checkout. The production transport is qualified on Linux; the portable client also has macOS and Windows source support, not a qualification result for those platforms.

## Configure access

- On the Linux edge, set `--quic-host edge.example.com --quic-cert chain.pem --quic-key key.pem`. The name must match the TLS certificate and resolve to the edge. Allow native QUIC and WebRTC on the **same advertised UDP port**. Do not assign QUIC a second UDP port. The HTTPS API still uses its normal endpoint and certificate.
- Obtain a short-lived participant token from your application backend. It needs access to join the room and discover and subscribe to the publication. Keep signing keys on the backend. Pass the token in `PULSEBEAM_TOKEN`, not as a command argument.
- Set `PULSEBEAM_LOCAL_IP` to this host's local interface address in the edge's IP family, not a wildcard address.
- Obtain the publication ID while the upstream publisher is live. Select `opus`, `h264`, `vp8`, `vp9`, or `av1` to match the publication. The worker supplies the stock codec format and room as `RtpSource` facts for the pinned ID. It does not transcode or fall back to another codec. For a custom publication, supply its exact signaled format and source room in your Rust client. The QUIC CA file contains trusted PEM roots for the advertised authority. The HTTPS placement client uses its normal trust roots; set `PULSEBEAM_PLACEMENT_CA_DER` to add a private HTTPS CA if needed. Neither connection disables hostname verification.

## Start recording

```sh
./bazel run //agents/pulsebeam-agent-quic:encoded_worker -- \
  https://api.example.com stage opus "$PUBLICATION_ID" /path/to/quic-ca.pem \
  record ./capture
```

Use a directory that does not yet exist. The worker needs a publisher sending media. If the directory contains only `index.tsv`, check access, the publication ID, the selected IP family, and UDP reachability.

`index.tsv` lists units, gaps, handoff loss, and binding resets. Each `unit` row names one payload file:

| Codec | File | Encoded bytes |
| --- | --- | --- |
| Opus | `.opus` | One raw Opus packet, not an Ogg file |
| H.264 | `.h264` | One Annex B access unit |
| VP8 | `.vp8` | One frame, partitions concatenated in order without RTP descriptors |
| VP9 | `.vp9` | One spatial frame without the RTP descriptor or a superframe index |
| AV1 | `.obu` | One spatial frame in low-overhead OBU format with explicit size fields |

The original columns retain their meaning. Sequence bounds are inclusive; a loss summary can enclose several intervals. `rtp_time` is extended presentation time in `clock_hz` ticks. Multiple spatial frames can share it; keep their row and sequence order. The trailing video columns give the DD frame number, spatial and temporal IDs, and the active and present target masks where known. `-` means unknown or not applicable. A sender-report anchor includes its RTP coordinate and a hex NTP value. It is not proof of synchronized capture across tracks. A new `epoch` resets sequence, timing, and descriptor context. Recording preserves complete dependent video and reports holes instead of synthesizing audio or video.

Replace `record ./capture` with `fresh` to run the encoded-unit callback. It counts units and prints a summary on exit. A real callback can inspect `Unit::data` and timing; move blocking work to a bounded sink. Freshness mode expires media after 150 ms of local residence and requests refresh when it needs a new independent video unit. The 150 ms is not an end-to-end latency guarantee.

### Backpressure and shutdown

Recording waits up to 120 ms for reorder; freshness waits up to 35 ms. Per binding, the host bounds reorder storage to 128 packets or 256 KiB, assembles video units up to 512 KiB, and queues up to 32 events or 1 MiB of encoded payload. At most 16 timed tracks can be attached. The example attaches one track and hands recording to a single storage thread with four pending items. A stalled sink does not stop the transport loop. If the reader queue fills, the index gets a loss summary once the sink can read again. If the filesystem fails, the worker reports the error and disconnects. Recording stops at 100,000 units or 1 GiB including index rows.

Press Ctrl+C to disconnect and drain work already handed to storage, each with a three-second deadline. The worker does not promise to write all media still buffered in the transport or reader. It does not retry filesystem failures, repair media holes, mux files, or guarantee crash durability. A partial tail can remain after failure or a drain timeout.

## Compose the Rust API

After joining a room and installing your publication or subscription intent, drive `agent.turn().await` on the owning reactor. Do not await a turn without first polling the output you need. Use `timed::Reader` with a `Policy` for complete ordered units; it resolves a replacement binding and discards old reader state. For direct control, attach a policy to the exact `Binding` returned by `agent.rtp_subscription` and read it with `next_encoded`. Other bindings can still use the raw path.

Select `Agent::with_raw_rtp()` before connecting to receive RTP packets without codec assembly. `next_rtp()` yields `rtp::Receive::Packet(packet)` or an explicit overflow; access the packet's sequence, timestamp, marker, extensions, and payload without RTP header reconstruction. When sending an existing logical RTP packet, resolve `agent.rtp_publication(kind, track_id)` and call `agent.send_rtp(&publication, send_packet)`. Use the source's sequence and timestamp, not the subscriber's. Resolve the publication again after a binding replacement.

For encoded publish, packetize at a logical RTP budget independent of the QUIC path. For example, on an installed Opus publication:

```rust
use pulsebeam_agent_quic::media::{Opus, PacketBudget};

let packetizer = Opus::new(PacketBudget::new(1200, 12).expect("header fits"));
let packet = packetizer
    .packetize(sequence, timestamp, false, encoded_opus)
    .expect("encoded packet fits the logical budget");
let result = agent.send_rtp(&publication, packet.as_send_packet(&[]));
```

For H.264 Annex B, use `media::H264::packetize` and send the resulting packets in order. To publish Dependency Descriptors (DD), call `agent.enable_h264_dependency_descriptors(track_id)` before connecting. Use `media::descriptor::Writer` with the encoder's dependency facts and pass its extension to `send_rtp` on each packet. Reset the writer on binding replacement. The default H.264 publication is DD-free. On temporary transport backpressure, retain the same packet and its descriptor, drive `turn`, and retry while the binding is live. The framer decides Whole or Slice carriage; the size in the snippet is not a UDP payload size or a path discovery setting. The [native QUIC wire contract](native-quic-v1.md) defines the fixed transport budget.

### Publish VP8, VP9, or AV1

Before joining, select the codec with `agent.configure_video_codec(track_id, profile)` and install a video `PublicationIntent` for that track. The raw QUIC publisher API in this example supports a single primary, unlayered stream for these codecs. Resolve the exact installed publication with `rtp_publication(MediaKind::Video, track_id)`; do not send until it is ready. For VP8, supply the actual partitions from the encoder, in order:

```rust
use pulsebeam_agent_quic::media::{PacketBudget, vp8};

let budget = PacketBudget::new(1200, 12).expect("header fits");
for packet in vp8::packetize(budget, first_sequence, rtp_timestamp, partitions)? {
    // On temporary backpressure, retain this packet, drive the agent, and retry.
    agent.send_rtp(&publication, packet.as_send_packet(&[]))?;
}
```

The snippet propagates errors for brevity. In a publishing loop advance the sequence only for admitted packets, and take the 90 kHz RTP timestamp from the source's presentation time. For VP9 supply the encoder's picture ID, prediction and reference facts to `media::vp9::packetize`. For AV1 supply low-overhead OBUs and mark a new coded video sequence only where the encoder starts one and supplies a sequence header. Spatial frames in one picture share an RTP timestamp. Set the marker on the last packet of the final transmitted spatial frame, not the last packet of every spatial frame. For a negotiated DD-emitting path, `media::descriptor::Publisher::write_if` commits frame and packet coordinates only if the caller accepts the packet with its extension; pass real encoder dependencies on the first packet of each spatial frame. Reset the publisher and reacquire the binding on replacement.

For DATA, set up the publication or subscription with `agent.resource` and submit a complete unit to `send_data` or `send_data_with_options`. Drive `turn` and consume `next_notification` for payload and completion events. Select Reliable for application receipts and bounded repair, Unreliable for gap reporting without repair, or Latest to supersede obsolete units. Admission to the local queue is not a remote receipt. Use separate media and DATA bindings.

To build a receive pipeline without the host policy, feed raw packets into `media::jitter::Buffer`, pass gap events to `media::units::Depacketizer::gap`, and use `ReceivedPacket::ordered` to adapt ordered output for `Depacketizer::push`. Attach sender timing only from `agent.rtp_sender_report(binding)` for the same binding. Apply `VideoRecovery` optionally; retry its PLI with `request_rtp_keyframe(binding)` only while that binding is current. See [`examples/encoded_worker.rs`](../../../agents/pulsebeam-agent-quic/examples/encoded_worker.rs) for the complete client loop and bounded sink.
