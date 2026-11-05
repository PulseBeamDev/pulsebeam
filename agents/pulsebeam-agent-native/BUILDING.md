# Build and publish the native agent

## Build from source

Use a Linux x86_64 repository checkout with the [host prerequisites](../../docs/build.md#host-prerequisites). Run these commands from the repository root. Bazel acquires the pinned Rust and WebRTC toolchains; Cargo is not a supported standalone build path yet.

Build the Rust library:

```sh
./bazel build //agents/pulsebeam-agent-native:pulsebeam-agent-native
```

Build the shared/static libraries and generated Swift, Kotlin, and Python bindings:

```sh
./bazel build //agents/pulsebeam-agent-native:native
./bazel cquery --output=files //agents/pulsebeam-agent-native:native
```

The query lists the library outputs and generated-bindings directory. The first native build downloads and compiles WebRTC; unchanged native inputs reuse Bazel's cache. Encoded media input still uses libwebrtc. The default is headless; desktop-device builds use the [sys package's graph-wide flavor](../../crates/pulsebeam-libwebrtc-sys/README.md#build-and-test). Binding generation does not qualify additional runtime platforms.

Build the Node 22 npm archive and Python 3.12 wheel:

```sh
./bazel build //agents/pulsebeam-agent-native:node_package.pack \
  //agents/pulsebeam-agent-native:python_wheel
```

The outputs are `bazel-bin/agents/pulsebeam-agent-native/node_package.tgz` and the wheel in `bazel-bin/agents/pulsebeam-agent-native/python-wheel/`. They target Linux x86_64 with glibc 2.39 or newer. Both contain the native library and notices; the Node archive declares pinned registry dependencies, including the prebuilt Node addon. See the [installation guide](README.md#install-the-generated-archives).

## Check the build

```sh
./bazel test //agents/pulsebeam-agent-native:unit_tests \
  //agents/pulsebeam-agent-native:clippy \
  //agents/pulsebeam-agent-native:format \
  //crates/pulsebeam-cli:production_native \
  //crates/pulsebeam-cli:installed_foreign
./bazel build //agents/pulsebeam-agent-native:pulsebeam-agent-native-sim
./bazel test //agents/pulsebeam-agent-native:generated_api_contract
```

The generated API contract executes direct Node and Python consumers of the same native metadata and generator outputs used by the packages. These local witnesses do not require an npm archive, wheel or registry publication, and do not qualify mobile execution.

`production_native` exercises the fast Rust/real-SFU boundary. `installed_foreign` installs both archives into isolated directories and exercises their generated APIs against the same local production SFU. These are focused native checks, not full release qualification. Follow the [support scope](QUALIFICATION.md) and [repository testing contract](../../docs/build.md#testing-contract) for affected and complete acceptance gates. Change Rust definitions and regenerate bindings; do not edit generated files.

## Validate the Node and Python releases

The package versions in `package.json` and `pyproject.toml` must agree. Before creating a release tag, validate the exact version and run complete acceptance:

```sh
./bazel run //release/npm:plan -- --tag npm-agent-v0.3.2
./bazel run //release/pypi:plan -- --tag pypi-agent-v0.3.2
./bazel run //release/pypi:validate -- --tag pypi-agent-v0.3.2
./bazel test //:test --test_output=errors
```

Adjust both tags to the checked-in package version. The PyPI validator inspects the actual wheel, including identity, native library and notices. The npm release contract checks the packed archive and release identity. Neither planning nor validation publishes anything.

The `npm-agent.yml` and `pypi.yml` workflows validate matching tags and retain the resulting artifacts. A tag push does not publish these packages. Publication is a separate manual workflow dispatch with `publish` enabled and the workflow ref set to the same release tag (for example, `gh workflow run npm-agent.yml --ref npm-agent-v0.3.2 -f tag=npm-agent-v0.3.2 -F publish=true`). Do this only after registry ownership, trusted-publisher configuration, environment approval and release readiness have been confirmed. The publication job consumes the validated artifact and uses GitHub OIDC. Configure npm's trusted publisher for `npm-agent.yml` and PyPI's for `pypi.yml`; never put registry tokens in these workflows. Registry access, tag creation and publication require separate authorization.

## Prepare a crates.io release

This crate is unpublished. Listing package files does not establish that an external Cargo application can build or link it:

```sh
cargo package --package pulsebeam-agent-native --list --locked
```

Before publishing:

- Make the complete dependency closure registry-resolvable and publish required internal dependencies first. `pulsebeam-core` and `pulsebeam-webrtc-sys` currently disable publication; core also declares a Git-only optional transport dependency that must be resolved for registry distribution.
- Provide and verify a standalone Cargo native build/link path. The current sys package is Bazel-only; package metadata does not supply its native libraries.
- Ensure protocol generation inputs and tools are available to downstream builds. `pulsebeam-proto` currently invokes `protoc` through `prost-build`.
- Build and run a separate application against the packaged crate, outside this workspace, including real media and shutdown. Confirm the documented platform prerequisites and include all required license notices.
- Confirm registry ownership, credentials, release versions, and the applicable acceptance gates.

After those prerequisites are satisfied, validate the package, then publish deliberately:

```sh
cargo publish --package pulsebeam-agent-native --dry-run --locked
cargo publish --package pulsebeam-agent-native --locked
```

Neither command replaces standalone-consumer verification. Do not use `--no-verify` to bypass native build or dependency failures.
