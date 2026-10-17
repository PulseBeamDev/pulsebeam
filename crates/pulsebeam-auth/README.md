# `pulsebeam-auth`

Transport-free identity and authentication primitives shared by `pulsebeam-core`
and the Rust server SDK. This crate owns canonical PulseBeam ID parsing and formatting,
external-ID validation, Ed25519 PKCS#8/SPKI key types, private API key validation,
standard key imports/exports, and redaction. It depends on neither core nor an SDK.

The Rust server SDK owns participant-JWT generation and its public signing API.
Core owns token verification, the single-project public key registry, typed entity IDs and their
hierarchical derivation. CLI policy and credential presentation stay in the CLI. Applications consume the documented `pulsebeam-server` API, not this
implementation dependency.

The SDK source archive includes this crate under `vendor/pulsebeam-auth` for
standalone local consumption. Both crates have explicit manifests and
Apache-2.0 licenses. Registry publication remains deferred; a future registry
release must make this versioned dependency available before publishing the SDK.

See [shared conformance](../../server/auth/README.md) for the wire authority and
[the Rust SDK](../../server/rust/README.md) for signing and package consumption.
Focused verification: `./bazel test //crates/pulsebeam-auth:unit_tests
//crates/pulsebeam-auth:format //crates/pulsebeam-auth:clippy`.
