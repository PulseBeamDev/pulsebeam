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

## Check the build

```sh
./bazel test //agents/pulsebeam-agent-native:unit_tests \
  //agents/pulsebeam-agent-native:clippy \
  //agents/pulsebeam-agent-native:format \
  //crates/pulsebeam-cli:production_native
./bazel build //agents/pulsebeam-agent-native:pulsebeam-agent-native-sim
```

These are focused native checks, not full release qualification. Follow the [support scope](QUALIFICATION.md) and [repository testing contract](../../docs/build.md#testing-contract) for affected and complete acceptance gates. Change Rust definitions and regenerate bindings; do not edit generated files.

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
