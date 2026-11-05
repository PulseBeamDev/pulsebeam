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

Ordinary `./bazel build`, `test` and `run` commands automatically start and
share a local NativeLink service across worktrees. Shared REAPI execution is
the default for WebRTC builds; no special config or separate startup is needed.
First use provisions the pinned service and host tools and may require network
access. Use this service only on a trusted single-user machine: its loopback
endpoints are unauthenticated. To build without the service, pass
`--//crates/pulsebeam-libwebrtc-sys:executor=local` to the command.

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

## Prepared media facts

`RtpTransceiver::applied_local_cname_bytes()` reads the exact sender's CNAME
from its currently applied native local description. RID-only streams retain
this value natively even when serialized SDP and `GetParameters` omit it.
The accessor returns no value before application or after close, preserves raw
bytes, and does not start a `GetParameters` transaction or alter its SDK snapshot.
Use `PeerConnection::set_local_description_auto()` to generate and apply the
SDK-owned local description before reading its signaling snapshot. Serializing a
RID offer and reparsing it through `set_local_description` discards CNAME facts
that SDP cannot represent.
Pass these actual observations to the shared core's local SDP context rather
than inferring CNAME from unrelated media.

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
patches. Acquisition can use the network. The locked Nixpkgs dependency
provisions package-local host tools, separate from the upstream target sysroot
and native desktop libraries.

`native_build.bzl` declares the whole native build action. It stages an
editable action-owned source view of declared upstream GN sources, adapters and
bridge, then invokes GN and Siso. The GN overlay compiles WebRTC, the owned
adapters and CXX-generated C++, with PIC, the software codec policy, protobuf
events and strict x86_64 floating-point behavior. `strict-fp.patch` preserves
all compile flags at the tail without a compiler wrapper. Bazel consumes fixed GN
archives for WebRTC, the bridge/additional roots and libc++/libc++abi.
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

The outer `WebRtcGn` action stays local (`no-remote-exec`) and retains Bazel
outputs and whole-build caching. Unchanged rebuilds and ordinary Rust, test or
documentation edits reuse its result. Bridge declarations in `src/bridge.rs`,
adapters, patches, pins, native tooling or GN configuration changes can rebuild
the native graph. Core, native and ASan retain distinct action/output identities.

By default Siso uses shared REAPI through the automatically started [local
NativeLink service](../../tools/nativelink/README.md). Siso uses pinned upstream
Chromium configs and package-local backend properties for `OSFamily=Linux` and
the exact Nix host-tools identity. REAPI's `strict_remote` prevents silent local
compile fallback. GN, Rust and Siso's local-only generators and archiving still
run on the client, with two local Siso jobs per client. The service is not a
governor for all machine processes.

Alternatively, `--config=webrtc-cache` uses the shared cache with local execution
and retains the whole-build guard. Only cache and REAPI modes permit the outer
action's REAPI network access. Use
`--//crates/pulsebeam-libwebrtc-sys:executor=local` for the explicit local
fallback, which runs Siso offline. See the service guide for the advanced foreground alternative,
custom loopback endpoints, recovery, and security and resource limits.
Additional platforms and a remote host fleet have not been verified.

The imported implementation is Apache-2.0 licensed. Upstream WebRTC patent and
license notices and CXX's dual-license notices are in `licenses/`; acquired
third-party sources retain their own notices.
