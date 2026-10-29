# `pulsebeam-steer`

Allocation-free, `no_std` client-ingress steering shared by SFU userspace and
the deterministic UDP steering adapter. No production dependencies are needed.

- `classify_bootstrap` extracts the STUN USERNAME's server ufrag and checks
  cluster, node and shard scope. A route is a candidate, not authorization.
- `steer_flow` selects an existing affinity mapping. Misses and invalid or
  stale mappings pass to userspace; classification never creates a flow.
- `TransportRoute::try_new` and `IceUfrag::try_new` reject fields outside their
  wire widths. `IceUfrag::new` is for trusted fields and panics on an invalid
  cluster id.

The v0 ufrag is 10 big-endian bytes encoded as 16 Crockford base32 characters:
`version(4) | cluster(12) | node(16) | shard(12) | slot(20) | epoch(16)`.
`FlowKey` retains its 40-byte C layout and native-endian map encoding.
STUN extraction is bounded by the declared 16-bit message length, accepts a
trailing packet suffix and uses the first USERNAME token before `:`.

Callers own sockets, flow storage and eviction, live route/epoch validation,
ICE authentication, admission and shard execution. Link framing is owned by
`pulsebeam::route`, not this crate. Encrypted link headers cannot be inspected
by a pre-decryption classifier. There is no production eBPF consumer or verifier
qualification.

Run `./bazel test //crates/pulsebeam-steer:unit_tests //crates/pulsebeam-steer:format //crates/pulsebeam-steer:clippy`
for codec compatibility, malformed packet boundaries and affinity fallback.
