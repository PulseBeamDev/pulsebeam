# Build and development

Bazel is the supported compilation, generation, and test graph. Run commands
from the repository root. Normal development and full browser acceptance support
Linux x86_64. Linux aarch64 runners support release binary assembly only.

## Host prerequisites

Use a writable checkout/cache and ordinary non-root execution on Linux. Install
[Nix](https://nix.dev/install-nix), with access to its normal `/nix/store`, and
make its `nix` and `nix-build` commands available on PATH. The clean reference
baseline is Ubuntu 24.04 with its standard shell utilities, Git, curl and CA trust,
plus Nix. No distro package-manager setup, host language/compiler installation,
or browser-library installation is part of the supported workflow.

`./bazel` verifies the pinned Bazelisk binary selecting `.bazelversion` and
starts Bazel directly on the host. It does not enter a private OS namespace,
change process libraries, or establish a runtime identity/daemon protocol.

`rules_nixpkgs_core` imports OS packages from `tools/host/flake.lock`.
`tools/host/sdk.nix` supplies concrete glibc startup objects, C/system headers,
zlib, GCC runtime libraries/private archives, and unwrapped mold. GCC executables
are not exposed. The small SDK projection uses Nixpkgs' `buildEnv` and LLVM's
supported sysroot interface. C++ headers and static libc++ remain supplied by
Bazel's LLVM 20.1.8 distribution. Compile/link actions declare the SDK files;
mold is a declared linker input selected by its immutable store path. The bounded
`native.bzl` bridge exists because toolchains_llvm 1.10.0 accepts a linker path,
not an artifact label. There is no ambient compiler or linker fallback.

Browser archives and drivers retain their URLs, versions and checksums from
`tools/browser-matrix.json`. Their separate `tools/host/browser.nix` package uses maintained
Nixpkgs `autoPatchelfHook` to supply each ELF executable/helper's interpreter
and shared-library RPATHs. Test targets consume the patched trees as declared
runfiles. Nix libraries are not injected into the host process via
`LD_LIBRARY_PATH`, and no outer FHS/container environment is required.
Nix store outputs remain rooted by the upstream repository integration while
Bazel uses them. Missing Nix, download failures, and unresolved libraries fail
provisioning rather than selecting host substitutes or skipping acceptance.

Bazel provisions versioned compilers, Rust libraries, Node/pnpm, Python, Go,
protobuf, native build tools, generators, browsers and drivers. Nix does not
compile PulseBeam or provide another build/test graph. No host language compiler,
package preparation sequence, development container, or language/browser cache
is required. Initial downloads need network access and CA trust; tests use local
fixtures and loopback services, not hosted application services.

To deliberately update the OS input, use the owning flake and review its lockfile:

```sh
nix --extra-experimental-features 'nix-command flakes' flake update --flake path:./tools/host
```

macOS/iOS CI is future work. It needs separate platform validation and Apple's
Xcode/SDK provisioning; the Linux SDK is not a macOS environment.

Missing libraries or failed pinned downloads are errors, not reasons to skip
acceptance. Browsers need working user namespaces or their documented headless
sandbox mode, a writable temporary directory, and available loopback ports.
Privileged perf/network diagnostics and a Docker-compatible daemon are optional
and not prerequisites for the complete test gate. The image loader consumes a
Bazel-built artifact and an optional Docker client/daemon; it does not build images.

## Shared local action cache

`.bazelrc` enables Bazel's native disk cache at
`~/.cache/pulsebeam/bazel/disk-cache`. Bazel expands `~` using its JVM user-home
property. This persistent, per-user directory is shared by local worktrees and
concurrent agents. Bazel supports concurrent cache readers/writers and validates
artifact digests; no cache daemon, wrapper protocol or remote service is needed.

Output state is **not** shared: the native workspace-hashed `output_base` retains
separate servers, analysis state, execution roots and locks for each worktree.
Do not configure a common `--output_base` or copy one worktree's output state into
another. `clean --expunge` removes worktree output state, not the shared cache.

Declared SDK/browser files, toolchains and action arguments remain part of the
action keys. The cache does not weaken checks or make results from a different
runtime reusable. Disk-cache GC size/age policies are disabled by default; use
Bazel's `--experimental_disk_cache_gc_max_size` or
`--experimental_disk_cache_gc_max_age` if a local retention limit is needed.
`--disk_cache=` explicitly disables caching for diagnostics.

## Entrypoints

```sh
./bazel build //:server //:cli //:native //:web //:react //:meet //:docs
./bazel test //:fast --test_output=errors
./bazel test //:test --test_output=errors
```

`//:fast` includes formatting, compiler/type/architectural/artifact checks and
fast owner tests, including controlled native RTC interoperability and the server
SDK contracts. `//:test` adds committed slow simulations and consumer-owned
Web/React browser contracts. Ignored exploratory cases and advisory seed search
retain their separate semantics.
The sole human-authorized gate exclusion is the known native DTLS reconnect
case, recorded in the [exception document](https://github.com/PulseBeamDev/pulsebeam/blob/main/crates/pulsebeam-simulator/docs/native-dtls-exception.md).
It is reported as skipped, not passed, and remains explicitly runnable.
`//:slow` selects the slow owners explicitly; do not run it per implementation
slice. Final acceptance runs the complete gate once against a frozen candidate.

Targeted selection uses the owning target and its harness:

```sh
./bazel test //crates/pulsebeam-routing:unit_tests --test_arg=--exact --test_arg=tests::example
./bazel test //crates/pulsebeam-simulator:fast --test_arg=--filter --test_arg=tests::bwe
```

Replace the example Rust name with a real test name. Simulator cases run in
separate processes, retaining virtual-clock/RNG isolation and committed seeds.
Simulation libraries retain optimization level 2, debug assertions and overflow
checks. Named `dev`, `release`, and `profile` configurations preserve the relevant
panic/assertion settings; use those build configurations for runtime binaries,
not abort-panic settings for libtest.

## Local development

```sh
./bazel run --config=dev //:server -- --dev
./bazel run //:web_dev -- --port 4173
./bazel run //:meet_dev -- --port 3000
./bazel run //:docs_dev -- --host 127.0.0.1
./bazel run //apps/meet:preview -- --listen 3000
./bazel run //docs:preview -- --host 127.0.0.1
```

Arguments after `--` go to the application/tool. The Web example is served from
built SDK/example assets. Meet defaults to `http://localhost:7070` in development;
set `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` to override it. Its static production export
uses `https://demo.pulsebeam.dev`. Docs retain VitePress/Mermaid behavior.
Generated dependencies are graph edges, not a required RTC-before-Web or
Web-before-React preparation order. Outputs live under `bazel-bin`.

## Formatting, generation and incremental changes

```sh
./bazel run //:fix
./bazel build //crates/pulsebeam-proto:pulsebeam-proto
./bazel build //agents/pulsebeam-agent-native:bindings
./bazel build //agents/pulsebeam-agent-web:uniffi_bindings
```

`fix` explicitly edits source using provisioned Rust/JS formatters. Normal
builds/tests never write tracked generated sources. Rust interfaces, `.proto`
files, manifests, generator configuration and locked dependencies are declared
inputs; change those rather than generated output. SDK and application outputs
rebuild transitively. An unchanged build reuses completed generation actions.

Tool/dependency authority is described in [the integration inventory](https://github.com/PulseBeamDev/pulsebeam/blob/main/tools/README.md).
After changing authoritative Rust manifests, toolchain/generator pins or lock
metadata, run `./bazel run --lockfile_mode=update //tools:update-rust-pins`, then
`./bazel mod deps --lockfile_mode=update`, and review the derived files.
Normal commands reject stale lock metadata rather than rewriting tracked files.
Refresh [editor assets](ide.md) after dependency/generated-input changes.
Remote caches and execution are optional optimizations, never correctness inputs.

## Simulation and performance utilities

```sh
./bazel run //:replay -- --seed 4711 --filter tests::bwe
./bazel run //:sweep -- --seeds 24 --from-seed 1 --filter tests::bwe
./bazel run //:scoreboard
./bazel run //:benchmark -- --save-baseline main
./bazel build --config=profile //:server
```

Replay and sweeps select explicit seed windows. Sweeps are advisory, print
reproduction commands, and do not weaken committed merge gates. `scoreboard`
explicitly updates `bwe-baseline.txt`; review its diff. Criterion reports remain
under `target/criterion`. Profiling builds retain optimized code, symbols and
frame pointers. The server's `:6060` metrics/pprof endpoints remain available.
Capture a profile with an OS HTTP client and inspect it using:

```sh
./bazel run @rules_go//go -- tool pprof -http=:8080 cpu.pprof
```

Kernel perf privileges, CPU affinity, packet capture and network configuration
remain optional manual procedures. They are never requested by an ordinary gate.
