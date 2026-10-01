# `pulsebeam-core`

Portable protocol and media helpers shared by the PulseBeam server, clients,
and simulator. It contains Dependency Descriptor parsing, H.264 and simulcast
helpers, canonical server identity and authentication types, the public project
registry, framing, and the abstract network interfaces used by real and
simulated transports. Shared ID/key codecs come from
[`pulsebeam-auth`](../pulsebeam-auth/README.md), with existing core type paths
preserved. Participant-JWT generation belongs to the Rust server SDK; core does
not depend on an SDK. Development-token orchestration belongs to the CLI and
test fixtures, not core.

The crate must remain independent of the server's shard runtime. Types here may
be used on either side of the wire, so parsing must validate hostile input and
avoid assumptions tied to one executor or operating system.

Run `./bazel test //crates/pulsebeam-core:unit_tests` for focused work and root `./bazel test //:test`
gate for changes consumed by the full stack.
