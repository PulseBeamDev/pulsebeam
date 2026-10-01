# @pulsebeam/server

Server-only Node.js 22+ SDK with native Ed25519 signing. No runtime dependencies,
PulseBeam connectivity, Rust, or WASM are needed. ESM and TypeScript are supported.
Public registry publication is not part of this change.

## Local installation

From the repository root:

```sh
./bazel build //sdks/typescript:package.pack
# In your application:
npm install /absolute/path/to/pulsebeam/bazel-bin/sdks/typescript/package.tgz
```

Use the documented public import, never an internal auth module:

```ts
import { signParticipantToken } from "@pulsebeam/server";

const projectId = process.env.PULSEBEAM_PROJECT_ID!;
const keyId = process.env.PULSEBEAM_KEY_ID!;
const secret = process.env.PULSEBEAM_SECRET!;
const token = signParticipantToken({
  projectId, keyId, secret,
  room: "general",
  participant: "Alice",
  expiration: 2000000000n,
});
```

Credentials must be supplied explicitly. The SDK does not read the environment.
Expiration is mandatory absolute integer Unix seconds, not a TTL. Safe integer
`number` values or `bigint` in `0..18446744073709551615` are accepted. Use bigint
for larger values, for example `18446744073709551615n`. Floats, unsafe numbers,
booleans, strings and missing inputs throw without returning a token. No clock
is consulted: zero or past expiration can be signed, but the server rejects a
token at and after its expiration.

Room and participant are case-sensitive external IDs of 1–36 ASCII characters
in `[A-Za-z0-9_-]`. Project/key IDs must be core V0 UUID v7 identities. The secret
must be an `sk_0...` 32-byte seed, not a public or expanded private key. Accepted
Crockford aliases are canonicalized for credential IDs only.

Keep secrets exclusively on your server. Send only participant tokens to clients,
and never log credentials. Offline signing cannot check registration or whether
the secret belongs to the supplied project/key. The server does that. SDK errors
do not include secret values. There is no public verification or credential API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //sdks/typescript:fast`.
