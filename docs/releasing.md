# Distribution and authorized automation

Compilation and non-publishing artifact assembly are Bazel-owned. Publication
is a separate credentialed operation, invoked only by the existing authorized
workflow triggers. Ordinary builds/tests never publish or require secrets.

## Linux GNU binaries and installer

```sh
./bazel build --config=release //release:local //release:global
./bazel test --config=release //release:contract --test_output=errors
```

Run on the architecture-appropriate Linux runner: x86_64 or aarch64. Aarch64
release support does not expand the full development/browser host matrix.
The application version comes from the server Cargo manifest; the distribution
contract in `release/distribution.toml` owns target identities and the tag prefix.
Release tags identify the package and version: `pulsebeam-v<version>` for the
server distribution. Bare version tags and alternative package-tag forms are
intentionally unsupported. This package-scoped convention permits independent release schedules without
selecting unrelated packages that happen to share a version. npm SDKs use the
separate package-scoped tags documented below.

The plan rejects a tag that does not match both the prefix and manifest version:

```sh
./bazel run //release:plan -- --tag pulsebeam-v0.4.9
```

Use the current manifest version rather than copying the example tag.

Outputs under `bazel-bin/release/artifacts` retain these identities:

- `pulsebeam-<target>.tar.xz`, with `pulsebeam-<target>/pulsebeam`, README and LICENSE;
- the archive's `.sha256` checksum;
- per-target assembly metadata identifying the version, target and archive hash.

The archive is assembled by `rules_pkg` from the Bazel-built binary and declared
documentation inputs. A bounded action adds checksum and identity metadata.

`bazel-bin/release/global-artifacts` contains `pulsebeam-installer.sh`, its checksum
and the version/prerelease plan. The installer verifies the archive before
installing `pulsebeam` into `${CARGO_HOME:-$HOME/.cargo}/bin`, preserves Cargo's
environment convention and supports `--no-modify-path`. To update, run the
installer from the explicitly chosen release; it replaces the binary only after
successful download and verification. No separate updater or receipt is required.
An existing `pulsebeam-update` executable is no longer used or shipped; remove
it if present and use the selected release's installer instead.

`//release:elf_contract` checks GNU loader identity, the Ubuntu 24.04-or-lower
runtime symbol baseline and absence of Nix-store/build-directory runtime paths.
This checks artifacts, not merely a successful link in the provisioning runtime.

The release contract exercises archive/layout/hash checks, real installation,
replacement of an existing installation and checksum-failure atomicity against
local non-production fixtures. Its temporary HOME/CARGO_HOME and loopback services
are isolated. Installer fixture routing uses `PULSEBEAM_DOWNLOAD_URL`; do not
point acceptance at a live release.

## Runtime image

```sh
./bazel build --config=release //:image
./bazel run --config=release //release:load
```

The second command requires an explicitly available Docker-compatible daemon;
image construction itself does not. `rules_oci` combines the Bazel-built release
binary with the immutable `cc-debian13:nonroot` base. The image is linux/amd64,
uses UID/GID 65532, `/app` working directory and `/app/pulsebeam` entrypoint, and
exposes `3478/udp` and `7070/tcp`. Arguments remain application arguments:

```sh
docker run --rm pulsebeam:local --help
docker run --rm --net=host pulsebeam:local --dev
```

Docker/Podman runtime use and privileged networking are optional host operations,
not part of language-tool provisioning or the ordinary non-privileged test gate.
No independent container Cargo compilation graph remains.

## Non-publishing assembly acceptance

On Linux x86_64 with an explicitly available Docker-compatible daemon:

```sh
./bazel build --config=release //release:all
./bazel test --config=release //release:contract //release:elf_contract //tools:workflow_contract --test_output=errors
./bazel run --config=release //release:load
test "$(docker image inspect --format '{{.Config.User}}' pulsebeam:local)" = '65532:65532'
test "$(docker image inspect --format '{{.Architecture}}' pulsebeam:local)" = amd64
docker run --rm pulsebeam:local --help
```

Repeat binary/archive contract acceptance on the native aarch64 release runner.
None of these commands creates a GitHub release or pushes an image. The tag
workflow validates both architecture bundles, installer and the amd64 runtime
before publication. Its registry job loads the validated image artifact rather
than rebuilding after GitHub release creation. Wiring review also checks the
existing trigger and credential boundaries in the workflow sources.

## Automation boundaries

- CI uses the pinned launcher and one complete non-privileged Bazel gate after
  installing only OS prerequisites. Browser logs, SDP/scenario records and other
  test outputs are retained on failure.
- Main pushes and manual dispatch build/check Meet's static export and upload
  `bazel-bin/apps/meet/out` through the existing Pages environment/credentials.
- `pulsebeam-v*` tags trigger the release plan, complete checks, native architecture
  builds, checksum validation and image smoke checks before GitHub release creation. Stable and
  prerelease versions follow the plan; failed/incomplete assemblies cannot publish.
- The image workflow consumes the same Bazel-owned binary/image, checks non-root
  behavior and argument handling, and publishes only on authorized non-PR events.
  The registry remains `ghcr.io/pulsebeamdev/pulsebeam`; tags retain short/long
  SHA, main/default-branch latest and full version-tag semantics.
- Nightly seed windows advance deterministically with the run number. Failure is
  advisory and retains replay evidence, never a merge gate.

Validation of wiring and artifact assembly does not authorize production
GitHub/GHCR/Pages publication. A real publication needs separate human authorization
or the existing authorized automation trigger. Native SDK publication and new
signing infrastructure remain outside this workflow.

## npm SDKs

Bazel owns package compilation, tarball assembly, release identity checks, and
the checksum-pinned npm 11.6.4 CLI running on the pinned Node 22.14.0 toolchain.
The GitHub workflow only provides an authorized tag trigger, runner setup,
acceptance invocation, and OIDC credentials. Ordinary builds/tests never publish.

| Package | Version authority | Release tag |
| --- | --- | --- |
| `@pulsebeam/web` | `agents/pulsebeam-agent-web/package.json` | `npm-web-v<version>` |
| `@pulsebeam/react` | `agents/react/package.json` | `npm-react-v<version>` |
| `@pulsebeam/server` | `server/typescript/package.json` | `npm-server-v<version>` |

Tags must exactly match the corresponding manifest's version. Stable releases
publish with the `latest` dist-tag; prereleases use `next`, never `latest`.
Each tag publishes only its named package. Publish a compatible Web version
before React: React retains its declared runtime dependency, and the publisher
checks that the public registry can resolve it before uploading React.
Server releases do not change the browser packages.

Non-publishing preparation from the repository root:

```sh
./bazel build //agents/pulsebeam-agent-web:package.pack //agents/react:package.pack //server/typescript:package.pack
./bazel test //release/npm:contract //agents/react:fast //server/typescript:fast
./bazel run //release/npm:plan -- --tag npm-react-v0.1.0
```

Use the current manifest version, not necessarily the example above. Tarballs
are `bazel-bin/<owner>/package.tgz`. Contracts validate public export files,
README/license, registry-safe dependencies, and the compiled Web WASM. The React
consumer installs the packed React/Web artifacts into an isolated application,
resolves frozen third-party dependencies, imports the public JavaScript exports,
compiles the WASM, and typechecks its public API. Server conformance consumes its
packed artifact against the shared signing vectors.

### First publication and trusted-publisher setup

An npm organization administrator must grant the publishing identity access to
the `@pulsebeam` scope. npm requires a package to exist before a trusted publisher
can be configured. For each new package, an authorized human must first publish
the validated tarball with their npm identity. Publish Web before React. For
example, after the complete `./bazel test //:test` gate and explicit publication
authorization:

```sh
./bazel run //release/npm:npm -- login --registry https://registry.npmjs.org/
./bazel run //release/npm:npm -- publish "$PWD/bazel-bin/agents/pulsebeam-agent-web/package.tgz" --access public --tag latest --ignore-scripts --registry https://registry.npmjs.org/
```

Repeat for React and Server with their respective tarball paths. Use `--tag next`
for a prerelease. These commands are real credentialed publication, not local
acceptance. No login or bootstrap publication occurs during ordinary tests.

Then configure each package's npm trusted publisher with:

- GitHub organization/user: `PulseBeamDev`;
- repository: `pulsebeam`;
- workflow filename: `npm.yml`;
- environment: `npm`.

Create the matching GitHub `npm` environment with the desired required reviewers
and tag deployment restrictions. Use a public repository, public packages, and
GitHub-hosted runners for OIDC provenance. The workflow grants `id-token: write`
only to its publishing job; no stored npm token is required. The pinned CLI
supports npm's [trusted publishing](https://docs.npmjs.com/trusted-publishers/).
Remove obsolete write tokens after confirming OIDC publication works.

### Routine releases

Update the owning manifest version and dependent version requirements/lockfiles
as needed, complete repository acceptance and review, then integrate the release
commit into main. An authorized human creates and pushes the corresponding
package tag on that integrated commit. The `npm.yml` workflow validates the tag,
runs `//:test`, then invokes:

```sh
./bazel run //release/npm:publish -- --tag "$GITHUB_REF_NAME"
```

This target requires GitHub Actions OIDC, revalidates the packed artifacts, and
publishes the selected tarball to `https://registry.npmjs.org/` with public access,
provenance, an explicit channel, and lifecycle scripts disabled. A failing gate,
version mismatch, absent compatible Web dependency, or npm authorization failure
stops publication. npm versions are immutable; correct a failed release before
retrying the same unoccupied version, or bump the version if it already exists.
This setup does not push tags, create GitHub releases, or publish SDKs to other
registries.
