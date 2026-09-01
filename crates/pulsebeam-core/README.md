# `pulsebeam-core`

Portable protocol and media helpers shared by the PulseBeam server, clients,
and simulator. It contains Dependency Descriptor parsing, H.264 and simulcast
helpers, framing, and the abstract network interfaces used by real and
simulated transports.

The crate must remain independent of the server's shard runtime. Types here may
be used on either side of the wire, so parsing must validate hostile input and
avoid assumptions tied to one executor or operating system.

Run `cargo test -p pulsebeam-core` for focused work and the root `just test`
gate for changes consumed by the full stack.
