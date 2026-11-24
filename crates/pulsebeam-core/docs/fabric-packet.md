# FabricPacket wire reference

A native endpoint and edge exchange one canonical FabricPacket byte image per logical packet. Decode it using the agreement installed for the binding and direction. Do not use these bytes as RTP, SCTP, or inter-node WAN datagrams. The WAN adapter projects the same typed packet into the existing authenticated RTP window format. WebRTC continues to use its existing RTP and SCTP formats.

## Packet layout (major 1)

All multibyte fields are big-endian. Offsets are in bytes.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | Profile ID |
| 1 | 1 | Flags: bit 0 is the marker; bit 1 selects two-byte extension headers. Other bits must be zero. |
| 2 | 4 | Full 32-bit packet sequence, independent of DATA `unit_seq` |
| 6 | 4 | Full 32-bit media timestamp, in the profile clock and wrapping modulo 2³² |
| 10 | 2 | Extension block length in bytes |
| 12 | Variable | Extension block, followed immediately by the payload to the end of the image |

There is no padding. The entire image is at most 1,280 bytes. This is a logical bound, not a UDP size or a guarantee that the WAN RTP projection fits. There are no embedded route, SSRC, hop, binding, generation, or direction fields. The native envelope selects the binding; authenticated binding state owns source identity, sequence ownership, lifetime and routing. A decoded image must still pass source normalization. In particular, the edge validates the profile payload and binding state and assigns admitted timing before routing; decoding alone never authorizes data or media.

| Profile ID | Payload | Clock |
| --- | --- | --- |
| 96 | Opus RTP payload | 48 kHz |
| 97 | H.264 RTP payload | 90 kHz |
| 98 | AV1 RTP payload (existing Fabric assignment, not a native endpoint capability) | 90 kHz |
| 99 | Descriptor-free `DATA_V1` record | 90 kHz |

A media payload must be nonempty. The RTP marker is profile-specific; for DATA it identifies the last fragment and must agree with the record. A DATA payload is 22 to 1,058 bytes: 22 bytes of record header, up to 12 bytes of capture-clock fields, and up to 1,024 bytes of fragment data. Any dependency descriptor is in extension 2, not duplicated in the DATA payload. DATA capture nanoseconds and uncertainty refer to the installed source clock; extension 3 is a separate timestamp and may coexist with both. See the [DATA record codec](../../pulsebeam-proto/src/data_record.rs) for the existing record layout and fragment rules. Native transport slicing does not change those rules or identify a new DATA fragment.

## Extension registry

Extensions are ordered by strictly increasing ID, with no duplicates or padding. Use a one-byte header if every value is at most 16 bytes. The high nibble is the ID and the low nibble is the value length minus one. Otherwise use a two-byte header for every element: ID, then value length. An empty block uses the one-byte form flag. IDs 0 and 1 are not assigned here. IDs and meanings below do not change between registry revisions.

| ID | Minimum revision | Profiles | Value |
| --- | --- | --- | --- |
| 2 | 0 | All | Dependency Descriptor, 3 to 255 bytes for media and 3 to 96 for DATA |
| 3 | 0 | All | Absolute Capture Time, 8 or 16 bytes |
| 4 | 1 | Opus | RFC 6464 audio level, 1 byte |
| 5 | 1 | Video | Video orientation, low four bits of one byte |
| 6 | 1 | Video | Video Layers Allocation, validated VLA value |
| 7 | 1 | Video | Video content type, one byte (0 or 1) |
| 8 | 1 | Video | Color space, 4 or 28 bytes |
| 9 | 1 | Video | Playout delay, two packed 12-bit values (minimum ≤ maximum), 3 bytes |

## Binding compatibility

For each direction, both sides must agree on one profile and major. The binding selects the highest implemented registry revision in the overlapping inclusive ranges. It intersects the advertised supported masks with the selected profile and revision's registry. Bit *n* denotes ID *n* in the 16-bit mask. Every required bit from either offer must be in the result or installation fails. This rule rejects unsupported required features even when their names are unknown to the implementation. Required bits require capability, not occurrence on every packet; per-packet presence rules are set by the profile and source binding. Registry negotiation is not WAN timing negotiation.

Ignore unknown supported-only bits during negotiation. Never skip an unknown or unnegotiated extension received on an installed binding. A sender projecting onto another binding may omit unnegotiated advisory metadata (IDs 4 to 9) if not required there, and must recompute the shortest extension header form. It must not omit a present Dependency Descriptor or Absolute Capture Time; if the destination cannot represent either, projection fails. A non-overlapping revision range, incompatible major, or missing required feature must not trigger guessing or fallback to an older native framing.

A maximum DATA record with a 96-byte descriptor, both DATA capture-clock fields and 16-byte Absolute Capture Time uses 1,186 bytes (12 header, 116 extension block and 1,058 payload), before native binding overhead. It cannot fit the observed 1,162-byte QUIC DATAGRAM budget at a 1,200-byte UDP floor. Transport must preserve the full logical image rather than resize the record or strip metadata to fit a path.
