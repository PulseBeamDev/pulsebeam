# PulseBeam Rust server SDK

`pulsebeam-server` exposes native Ed25519 signing through
`pulsebeam_server::sign_participant_token`. Rust 1.92+ is required. There is no
PulseBeam connectivity, WASM, foreign runtime, or dependency on the server/core
crate. Public key-registry publication is handled separately by the PulseBeam server.

## Local development

`insecure_dev_api_key()` returns the publicly known API key from the
[shared development fixture](../auth/development.json), matching `--dev`
servers. Pass it to `sign_participant_token`. **Never use it with production or publicly accessible servers.**

## Local crate consumption

Use a checkout as a normal Cargo path dependency:

```toml
[dependencies]
pulsebeam-server = { path = "/absolute/path/to/pulsebeam/server/rust" }
```

Or build `./bazel build //server/rust:package`, unpack
`bazel-bin/server/rust/package.tar.gz`, and point the path dependency at that
directory. The archive contains the SDK manifest, sources, README and license,
plus its versioned `pulsebeam-auth` dependency under `vendor/pulsebeam-auth`.
The packaged manifest points inside the archive. Both crates have explicit Cargo
manifests and Apache-2.0 licenses, with no workspace inheritance or private
repository paths. A future registry release must publish the shared dependency
before the SDK; no registry publication is performed by this build.

```rust
use pulsebeam_server::{Action, Grant, GrantSet, sign_participant_token};

fn token(api_key: &str) -> Result<String, Box<dyn std::error::Error>> {
    let grants = GrantSet::new(vec![Grant::new(
        vec!["general".into()],
        vec![Action::Join, Action::Publish, Action::Discover, Action::Subscribe],
        None, None, None,
    )?])?;
    let issued = sign_participant_token(api_key, &grants, "Alice", 2_000_000_000)?;
    Ok(issued.token)
}
```

The opaque API key, grants, participant and expiration are mandatory. Grants
are validated on construction or deserialization; an empty `GrantSet` grants no access.
The SDK splits the API key and imports only the standard PKCS#8 Ed25519 private key;
the SFU validates identities. The SDK never reads the environment. Expiration
is a `u64` absolute Unix timestamp in `0..18446744073709551615`, not a TTL.
Rust rejects missing arguments, booleans, floats, strings, negative constants
and overflowing constants before signing. Validate externally supplied values
instead of casting them to `u64`. No clock is consulted. Zero and past expiration
may be signed, but authorize nothing at or after expiry.

Participant IDs match `[A-Za-z0-9_-]{1,36}` and preserve case. Grant room
patterns and labels are nonempty UTF-8 strings of at most 256 bytes,
without control characters. Case and exact text are preserved.
The SDK validates these bounds and grant structure. Project/key identifiers
remain opaque; the SFU validates their format, registry membership and authority.
See the [grant schema and limits](../auth/README.md#grants).
The SDK rejects unusable signing keys, and errors do not disclose secrets.

The signer returns `SignedParticipantToken` with `token` and its signed UUIDv4
`jti`. Send only the token to the client. Retain `jti` on your backend if you need
incident correlation; do not log the JWT.

Keep signing secrets on your server, never in clients or logs. Offline signing
cannot check registration or whether the supplied secret belongs to the
project/key. The server performs those checks. There is no public token
verification or credential-generation API. The transport-free
[`pulsebeam-auth`](../../crates/pulsebeam-auth/README.md) crate owns the shared
ID/key codecs. Core and this SDK depend on it independently; neither depends
on the other. Applications use the root SDK signing API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //server/rust:fast //server/auth:fast`. The artifact consumer compiles
the extracted source package with Bazel, imports its public crate, and matches
all shared vectors; compile-fail doctests prove the typed input boundary.
