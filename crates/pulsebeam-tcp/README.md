# pulsebeam-tcp

RFC 4571 framed I/O for Tokio `AsyncRead + AsyncWrite` streams, including
virtual-time socket adapters. Use `split(stream, max_frame_bytes)` to obtain
independent `FrameReader::recv` and `FrameWriter::send` halves.

Lengths are unsigned big-endian 16-bit values. Empty frames are legal and
distinct from EOF. Reads reject oversized lengths before allocating and report
truncated frames as errors. Cancelling a receive preserves partial input;
cancelling a send requires retiring the stream because it may have written only
part of a frame. `FrameReader::is_partial` distinguishes idle input from an
incomplete frame when the caller applies read deadlines.

This crate does not spawn tasks or choose runtimes, admission limits, queues,
routing, authentication, or media policy. Its owner checks are:

```sh
./bazel test //crates/pulsebeam-tcp:unit_tests //crates/pulsebeam-tcp:format //crates/pulsebeam-tcp:clippy
```
