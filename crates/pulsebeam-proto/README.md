# `pulsebeam-proto`

Protobuf wire definitions and generated Rust types for reliable signaling over
PulseBeam data channels. The crate also owns reserved RTP extension identifiers
and stable signaling topic names.

Edit the files in `proto/`, not generated output in `OUT_DIR`. Wire changes must
be reviewed for compatibility with every producer and consumer before the
generated Rust API changes.

Run `cargo test -p pulsebeam-proto` for focused verification, then the root
`just check` and `just test` gates.
