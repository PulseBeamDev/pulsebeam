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

## Configure an API key

Generate a project and API key with `pulsebeam-cli key`. The command prints the
private API key and public key-registry JSON; it does not create files or
configure your application.

Treat the API key as an opaque server-side secret. Your application decides how
to store it and passes it explicitly to this SDK. The production PulseBeam
server uses the corresponding public key registry for verification.

## Issue a participant token

```ts
import { signParticipantToken } from "@pulsebeam/server";

function issueToken(apiKey: string) {
  return signParticipantToken({
    apiKey,
    room: "general",
    participant: "alice",
    expiration: Math.floor(Date.now() / 1000) + 3600,
  });
}
```

The SDK signs locally without reading the environment itself or performing a
network request. Only the short-lived participant JWT goes to the browser.
A renewal must use the **same** room and participant identity.

For compatibility, `signParticipantToken` also accepts the original
`{projectId, keyId, secret}` configuration. The new single-key API produces
the same participant JWT; the SDK owns the credential representation.

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
