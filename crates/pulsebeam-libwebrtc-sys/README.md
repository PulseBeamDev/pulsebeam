# PulseBeam WebRTC bindings

`pulsebeam-webrtc-sys` owns PulseBeam's low-level Rust bindings, CXX bridge,
C++ adapters and WebRTC behavior patches. Use the higher-level
[native agent](../../agents/pulsebeam-agent-native/README.md) for application code.
Bazel is the supported build and test entry point. Cargo metadata supports
workspace dependency maintenance and IDE discovery, not standalone compilation.

## Build and test

Use Linux x86_64 with the repository's [host prerequisites](../../docs/build.md#host-prerequisites).
From the repository root:

```sh
./bazel build //crates/pulsebeam-libwebrtc-sys:pulsebeam-webrtc-sys
./bazel test //crates/pulsebeam-libwebrtc-sys:fast --test_output=errors
./bazel test //crates/pulsebeam-libwebrtc-sys:runtime_tests --test_output=errors
```

Core is the default and does not open platform audio or capture devices. Select
native for the entire configured dependency graph to enable the Linux desktop
backend:

```sh
./bazel test //crates/pulsebeam-libwebrtc-sys:runtime_tests \
  --//crates/pulsebeam-libwebrtc-sys:flavor=native --test_output=errors
```

There is one Rust dependency label for both configurations. Its Rust feature
cfgs and native GN inputs use the same package-owned flavor setting. Cargo
feature unification and per-dependency flavor labels do not select native code.
Tests requiring physical devices or external services are not default acceptance.
Other target platforms are not yet supported by the Bazel integration.

## Lifetime instrumentation

The focused teardown and callback suite can instrument the native implementation
with AddressSanitizer in either flavor:

```sh
./bazel test //crates/pulsebeam-libwebrtc-sys:lifetime \
  --//crates/pulsebeam-libwebrtc-sys:asan --test_output=errors
./bazel test //crates/pulsebeam-libwebrtc-sys:lifetime \
  --//crates/pulsebeam-libwebrtc-sys:asan \
  --//crates/pulsebeam-libwebrtc-sys:flavor=native --test_output=errors
```

These test targets use the matching declared Chromium Clang 23 linker and ASan
runtime. The application toolchain is unchanged. Rust code itself is not
instrumented by this native lifetime configuration.

## Build ownership and caching

`repositories.bzl` acquires the pinned WebRTC and depot_tools revisions, their
DEPS inputs, prebuilt tools and Bullseye amd64 sysroot, then applies the tracked
patches. Acquisition can use the network. Compile and link actions cannot.
The locked Nixpkgs dependency provisions package-local host tools, separate from
the upstream target sysroot and native desktop libraries.

`native_build.bzl` declares the whole native build action. It stages an
action-owned source view and invokes GN and Ninja. The GN overlay compiles
WebRTC, the owned adapters and CXX-generated C++, with PIC, the software codec
policy, protobuf events and strict x86_64 floating-point behavior. Bazel consumes
fixed GN archives for WebRTC, the bridge/additional roots and libc++/libc++abi.
The pinned Bullseye sysroot supplies target headers and link metadata, but its
shared libraries are stripped link-only stubs. Native desktop runtime libraries
come from checksum-pinned original packages in that sysroot's Debian snapshot,
with matching package versions and declared Bazel runtime dependencies.
There is no source/native candidate archive, SDK staging step, persistent Ninja
work directory or independently published native artifact.

CXX is pinned to 1.0.194 throughout. GN owns its host generator and C++ runtime;
Bazel compiles the upstream registry Rust runtime and proc macro with the
monorepo's Rust toolchain. Package-local dependency annotations disable Cargo's
native compiler and C++ linker-helper build scripts.

Bazel caches the entire native result. Unchanged rebuilds and ordinary Rust,
test or documentation edits reuse it. Bridge declarations in `src/bridge.rs`,
adapters, patches, pins, native tooling or GN configuration changes can rebuild
the whole native graph. Core, native and ASan configurations have distinct
action/output identities. Object-level incremental reuse, native remote
execution and additional platforms are not promised.

The imported implementation is Apache-2.0 licensed. Upstream WebRTC patent and
license notices and CXX's dual-license notices are in `licenses/`; acquired
third-party sources retain their own notices.
