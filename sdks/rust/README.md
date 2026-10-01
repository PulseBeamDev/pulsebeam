# PulseBeam Rust server SDK

`pulsebeam-server` exposes native Ed25519 signing through
`pulsebeam_server::sign_participant_token`. Rust 1.92+ is required. There is no
PulseBeam connectivity, WASM, foreign runtime, or dependency on the server/core
crate. Public registry publication remains deferred.

## Local crate consumption

Use a checkout as a normal Cargo path dependency:

```toml
[dependencies]
pulsebeam-server = { path = "/absolute/path/to/pulsebeam/sdks/rust" }
```

Or build `./bazel build //sdks/rust:package`, unpack
`bazel-bin/sdks/rust/package.tar.gz`, and point the path dependency at that
directory. The archive contains the SDK manifest, sources, README and license,
plus its versioned `pulsebeam-auth` dependency under `vendor/pulsebeam-auth`.
The packaged manifest points inside the archive. Both crates have explicit Cargo
manifests and Apache-2.0 licenses, with no workspace inheritance or private
repository paths. A future registry release must publish the shared dependency
before the SDK; no registry publication is performed by this build.

```rust
use pulsebeam_server::sign_participant_token;

fn token() -> Result<String, Box<dyn std::error::Error>> {
    let project = std::env::var("PULSEBEAM_PROJECT_ID")?;
    let key = std::env::var("PULSEBEAM_KEY_ID")?;
    let secret = std::env::var("PULSEBEAM_SECRET")?;
    Ok(sign_participant_token(
        &project, &key, &secret, "general", "Alice", 2_000_000_000,
    )?)
}
```

All six inputs are mandatory. The SDK never reads the environment. Expiration
is a `u64` absolute Unix timestamp in `0..18446744073709551615`, not a TTL.
Rust rejects missing arguments, booleans, floats, strings, negative constants
and overflowing constants before signing. Validate externally supplied values
instead of casting them to `u64`. No clock is consulted. Zero and past expiration
may be signed, but authorize nothing at or after expiry.

Room and participant are case-sensitive external IDs of 1–36 ASCII characters
in `[A-Za-z0-9_-]`. Project/key IDs must be core V0 UUID v7 identities. The
`sk_0...` secret is a 32-byte Ed25519 seed, not a public or expanded private key.
Credential aliases are canonicalized; external IDs are never normalized or
trimmed. Invalid string inputs return `SigningError` without a token. Errors
and default diagnostics do not disclose credential values or decoded seeds.

Keep signing secrets on your server, never in clients or logs. Offline signing
cannot check registration or whether the supplied secret belongs to the
project/key. The server performs those checks. There is no public token
verification or credential-generation API. The transport-free
[`pulsebeam-auth`](../../crates/pulsebeam-auth/README.md) crate owns the shared
ID/key codecs. Core and this SDK depend on it independently; neither depends
on the other. Applications use the root SDK signing API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //sdks/rust:fast //sdks/auth:fast`. The artifact consumer compiles
the extracted source package with Bazel, imports its public crate, and matches
all shared vectors; compile-fail doctests prove the typed input boundary.
