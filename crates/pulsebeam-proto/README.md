# `pulsebeam-proto`

Protobuf wire definitions and generated Rust types for reliable signaling over
PulseBeam data channels. The crate also owns reserved RTP extension identifiers
and stable signaling topic names.

Edit the files in `proto/`, not generated output in `OUT_DIR`. Native media
messages use one independent raw LZ4 block per protobuf envelope, with a
32,768-byte decoded and 32,912-byte wire limit. Wire changes must be reviewed
for every producer and consumer before the generated Rust API changes.

Run `cargo test -p pulsebeam-proto` for focused verification, then the root
`just check` and `just test` gates.
