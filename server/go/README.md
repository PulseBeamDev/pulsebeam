# PulseBeam Go server SDK

Import `github.com/PulseBeamDev/pulsebeam/server/go` (package `pulsebeam`). Go 1.24+
is required. Signing uses only the Go standard library: no PulseBeam service,
Rust, WASM, or foreign runtime is needed. Registry publication is deferred.

## Local module consumption

In your application's `go.mod`, require the module and replace it with a local
checkout until it is released:

```go
require github.com/PulseBeamDev/pulsebeam/server/go v0.0.0
replace github.com/PulseBeamDev/pulsebeam/server/go => /absolute/path/to/pulsebeam/server/go
```

```go
import (
    "os"
    pulsebeam "github.com/PulseBeamDev/pulsebeam/server/go"
)

token, err := pulsebeam.SignParticipantToken(
    os.Getenv("PULSEBEAM_PROJECT_ID"),
    os.Getenv("PULSEBEAM_KEY_ID"),
    os.Getenv("PULSEBEAM_SECRET"),
    "general", "Alice", uint64(2000000000),
)
if err != nil {
    // Handle without logging credentials. No token is returned on error.
    return err
}
_ = token
```

All credentials are explicit. The SDK never reads the environment. Expiration
is a mandatory uint64 absolute Unix timestamp in `0..18446744073709551615`, not
a TTL. Go's type checker rejects missing, negative, fractional, boolean,
string, or out-of-range arguments. Do not cast an inexact caller value to uint64;
validate external data before passing it to the typed API. No clock is consulted.
Zero and past expiration can be signed, but the server rejects at and after expiry.

Room and participant are case-sensitive external IDs of 1–36 ASCII characters
in `[A-Za-z0-9_-]`. Project/key IDs are core V0 UUID v7 identities. The `sk_0...`
secret is a 32-byte Ed25519 seed, not a public or expanded private key. Credential
aliases are accepted and IDs canonicalized; external identities are never trimmed.

Keep secrets on servers only, never in clients or logs. Offline signing cannot
check registration or whether a secret belongs to the supplied project/key.
The server performs registry matching and authorization. SDK errors contain no
secret values. There is no public verification or credential-generation API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //server/go:fast`. The `//server/go:package` artifact is a source module
archive, which can be unpacked and used in a local `replace` directive.
