# `pulsebeam-core`

Portable protocol and media helpers shared by the PulseBeam server, clients,
and simulator. It contains Dependency Descriptor and Video Layers Allocation
decoding, H.264 and simulcast helpers, canonical server identity and authentication
types, the public single-project key registry, framing, and the abstract network
interfaces used by real and
simulated transports. Shared ID/key codecs come from
[`pulsebeam-auth`](../pulsebeam-auth/README.md), with existing core type paths
preserved. Participant-JWT generation belongs to the Rust server SDK; core does
not depend on an SDK. Development-token orchestration belongs to the CLI and
test fixtures, not core.

The crate must remain independent of the server's shard runtime. Types here may
be used on either side of the wire, so parsing must validate hostile input and
avoid assumptions tied to one executor or operating system.

## Fabric packet wire contract

Use the [FabricPacket wire reference](docs/fabric-packet.md) to implement a
compatible native payload reader or writer. It defines the binding agreement,
packet layout and extension registry separately from the WAN RTP projection.

## Fabric packet timing

Fabric uses a single sampling timeline for audio, video, and DATA. At first
admission the publisher's shard assigns the timestamp as the floor of elapsed
continuous time since the Unix epoch instant, scaled to the media clock rate
and reduced modulo 2³². Opus uses 48 kHz; video and DATA use 90 kHz. The
assigned value is stable across packets of one sample or DATA unit, retries,
relays, and repairs. A relay does not convert it to its own clock. A weak or
missing source reference can produce a cadence-preserving estimate with unknown
absolute error; it does not stop otherwise valid traffic.

The typed shard packet has no ingress NTP stamp, and there is no serialized
local RTP envelope. The provisioned WAN RTP header carries the canonical
timestamp in its existing 32-bit timestamp field. For DATA, that field is not
`unit_seq`: record bytes and fragment identity are independent. The sender
binds timing context to the source incarnation and sequence anchor in
ownership control, not in an additional RTP extension. Missing, stale, or
nonmatching timing context is not a basis for a drop.

On endpoint egress, a continuous output epoch uses a fixed RTP offset from
canonical time. A Sender Report uses one instant for both the translated RTP
timestamp and its UTC/NTP label. An unavailable UTC label defers the report
without stopping RTP. Capture age and queue residence are not presentation
time; browsers and Native receivers still own playout.

## Fabric peer compatibility

Fabric timing revision 1 uses the existing WAN RTP header and extension
layout. In the DTLS-SRTP capability and the installed 17-byte window tuple,
the timing revision is `01`; it is not the extension registry revision.
A Fabric datagram association also exchanges an explicit timing revision
before it carries media or ownership control. A compatible capability only
selects the profile. Receiving or sending WAN media also requires a qualified
path and an active window for the current peer binding and direction.

Revision 0 is not supported. Upgrade Fabric peers as one release, restart
the associations, and discard old windows, buffered images, and repair
state. Compatibility with endpoint WebRTC does not depend on the internal
Fabric timing revision.

For qualified absolute timing, run the server nodes with disciplined, unsmeared
Linux UTC and TAI clocks and a current IANA `leap-seconds.list`. For embedded
nodes, configure this with `NodeBuilder::host_utc_policy`. Attest unsmeared UTC
only when the host discipline is configured for it. An unqualified reference
can forward media but must not be used as a bound on absolute capture error.

Run `./bazel test //crates/pulsebeam-core:unit_tests` for focused work and root `./bazel test //:test`
gate for changes consumed by the full stack.
