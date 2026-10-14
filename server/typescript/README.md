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

For local `--dev` servers, `INSECURE_DEV_CREDENTIALS` is loaded from
embedded JSON generated from the [shared fixture](../auth/development.json).
Never use it in production or expose a dev server publicly.

## Configure one API key

Set a **server-only** `PULSEBEAM_API_KEY` containing a V0 `key_0...` string.
Its 64-byte payload is project UUIDv7 (16), key UUIDv7 (16), and raw Ed25519
seed (32), encoded together using Crockford Base32. See the
[complete format and validation contract](../auth/api-key.md).

Use `pulsebeam-cli auth-key` to generate the public registry and private key:

```sh
./bazel run //:cli -- auth-key \
  --public-registry project-registry.json \
  --private-signing-bundle signing-key.json \
  --private-api-key pulsebeam.key
```

Configure the SFU with the **public** registry, and place the private key into
your backend's secret store as `PULSEBEAM_API_KEY`. Do not send it to browsers.
For local `--dev` only, use the publicly known `INSECURE_DEV_API_KEY`.

## Issue a participant token

```ts
import { signParticipantToken } from "@pulsebeam/server";

const token = signParticipantToken({
  apiKey: process.env.PULSEBEAM_API_KEY!,
  room: "general",
  participant: "alice",
  expiration: Math.floor(Date.now() / 1000) + 3600,
});
```

The SDK signs locally without reading the environment itself or performing a
network request. Only the short-lived participant JWT goes to the browser.
A renewal must use the **same** room and participant identity.

For compatibility, `signParticipantToken` also accepts the original
`{projectId, keyId, secret}` triple. The V0 API key packages those
same fields; the resulting JWT and public-key verification are unchanged.

The expiration must be an absolute safe-integer number or bigint within the
unsigned 64-bit range; it is not a TTL. Room/participant IDs are case-sensitive
1–36 ASCII characters in `[A-Za-z0-9_-]`. Offline signing cannot check public-key
registration or user permissions. Reject invalid keys and inputs without
printing sensitive values.

## Contributor checks

Run from the repository root:

```sh
./bazel test //server/typescript:fast
```

This checks the packed public package against the
[shared participant-token conformance profile](../auth/README.md).
