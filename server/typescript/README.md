# @pulsebeam/server

Mint participant bearer tokens on your Node.js backend for clients joining a
PulseBeam room. The SDK signs locally with native Ed25519; it does not connect
to PulseBeam at runtime. It supports ESM and TypeScript
on **Node.js 22 or newer**.

Keep signing secrets on your backend. Give clients only participant tokens,
after authenticating the caller and authorizing their room and participant ID.

## Install from source

`@pulsebeam/server` is not currently available from the public npm registry.
Using this package requires a source build; the browser SDKs' npm installation
does not install it.

Use a Linux x86_64 PulseBeam checkout with the
[host prerequisites](../../docs/build.md#host-prerequisites) installed. From the
repository root, build the installable tarball:

```sh
./bazel build //server/typescript:package.pack
```

Then, in your **backend application directory**, install that artifact with your
application's npm. Replace the path with your checkout's absolute path:

```sh
npm install /absolute/path/to/pulsebeam/bazel-bin/server/typescript/package.tgz
```

Bazel supplies the build tools; your backend still needs Node.js 22 or newer.
Maintainers can follow the [npm release guide](../../docs/releasing.md#npm-sdks)
to publish the package. Publication is not part of installation or testing.

## Development credentials

For local `--dev` servers, `INSECURE_DEV_CREDENTIALS` provides the
public test identity in [shared JSON](../auth/development.json).
Never use it in production or expose a dev server publicly.

## Configure credentials

You need a project ID, API key ID, and `sk_0...` signing secret whose public key
is registered with your compatible PulseBeam server. Obtain these from your
server operator, or create them from the same build-ready checkout:

```sh
./bazel run //:cli -- auth-key \
  --public-registry project-registry.json \
  --private-signing-bundle signing-key.json
```

Both output paths must be new files. The private bundle is owner-only on Unix.
Configure your production PulseBeam server's `--project-registry` option with the
**public** registry. Keep the **private** bundle on your backend, outside source
control and browser assets. Load its `project_id`, `key_id`, and `signing_key`
into your backend's secret store; the example below reads them as
`PULSEBEAM_PROJECT_ID`, `PULSEBEAM_KEY_ID`, and `PULSEBEAM_SECRET` respectively.
The SDK itself never reads environment variables or the bundle.

See the [CLI authentication commands](../../crates/pulsebeam-cli/README.md)
for key rotation. Development credentials and `--dev` are for local testing only.
Signing a token does not register credentials or prove that the server accepts them.

## Issue a token

In your backend application directory, save this as `sign-token.mjs`. Set the
three environment variables through your server-side secret store before running it:

```js
import { signParticipantToken } from "@pulsebeam/server";

function required(name) {
  const value = process.env[name];
  if (!value) throw new Error(`Missing ${name}`);
  return value;
}

const token = signParticipantToken({
  projectId: required("PULSEBEAM_PROJECT_ID"),
  keyId: required("PULSEBEAM_KEY_ID"),
  secret: required("PULSEBEAM_SECRET"),
  room: "general",
  participant: "alice",
  expiration: BigInt(Math.floor(Date.now() / 1000)) + 3600n,
});

console.log("Issued participant token for alice in general.");
```

```sh
node sign-token.mjs
```

**Success:** the script prints the confirmation above, and `token` contains a
signed participant JWT expiring one hour after issuance. It does not log the
token or credentials. In your backend, return `token` only to an authenticated,
authorized client; the client passes it to the Web or React SDK with your server's
HTTP(S) base URL. For a two-participant room, issue separate tokens with the same
room and different participant IDs.

A `Missing PULSEBEAM_...` error means the application has not supplied a required
environment variable. A `TypeError: Invalid ...` means an SDK input is malformed.
If a signed token is rejected on connection, check the server's public registry,
the project/key/secret correspondence, and expiration; generate a fresh token
with the matching registered credentials.

## Input and security reference

- All six inputs are required. Import only from `@pulsebeam/server`, not an
  internal auth module.
- `expiration` is absolute integer Unix seconds, not a TTL. Use a safe-integer
  `number` or `bigint` in `0..18446744073709551615`; larger-than-safe integers
  require `bigint`. Floats, unsafe numbers, strings, booleans, and missing inputs
  throw without returning a token. The SDK consults no clock: it can sign zero
  or past expiration, but the server rejects a token at and after expiration.
- Room and participant IDs are case-sensitive, 1 to 36 ASCII characters in
  `[A-Za-z0-9_-]`. These are application identities, not proof of an end user's
  identity or permission to join.
- Project/key IDs must be V0 UUID v7 identities with `p_` and `kid_` prefixes.
  The secret must encode a 32-byte Ed25519 seed with `sk_0...`, not a public key
  or expanded private key. Accepted Crockford aliases are canonicalized for
  credential IDs only.
- Offline signing cannot check registration or whether the secret belongs to
  the supplied project/key. The server verifies these and the signature.
  SDK errors do not include secret values; never log credentials or tokens.
  There is no public verification or credential-management API.

## Contributor checks

Run from the repository root:

```sh
./bazel test //server/typescript:fast
```

This checks the packed public package against the
[shared participant-token conformance profile](../auth/README.md).
