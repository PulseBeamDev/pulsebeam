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

Run `./bazel test //crates/pulsebeam-core:unit_tests` for focused work and root `./bazel test //:test`
gate for changes consumed by the full stack.
