# Build and development

Bazel is the supported compilation, generation, and test graph. Run commands
from the repository root. Normal development and full browser acceptance support
Linux x86_64. Linux aarch64 runners support release binary assembly only.

## Host prerequisites

Use a writable checkout/cache and ordinary non-root execution on Linux. Install
[Nix](https://nix.dev/install-nix), with access to its normal `/nix/store` and
unprivileged user namespaces, then use `./bazel`. Fedora and Ubuntu use the same
pinned OS inputs without distro-specific development-header or browser-library
installation lists.

`tools/host/flake.nix` supplies only the Linux OS environment: bootstrap utilities,
C/system headers and libraries, browser runtime libraries, the unwrapped mold
linker and Docker client for loading Bazel-built images. Only GCC's library
directories are projected into the runtime, including the static archives required
by its shared-unwind linker script; no GCC executable is exposed. C++ headers and
static libc++ come from the Bazel-provisioned LLVM distribution.
`tools/host/flake.lock` pins its Nixpkgs input. The maintained Nixpkgs FHS
runtime avoids custom sysroot-layout machinery; it is not a hermetic kernel or an
Apple SDK. The LLVM integration supports an executable linker path, so it selects
mold at the locked runtime's `/usr/bin/mold`, not an ambient host installation.

The launcher refuses implicit lockfile updates, enters that environment and
verifies its immutable `/etc/pulsebeam-host` marker against the flake, lockfile and
execution architecture. Caller-supplied environment flags cannot substitute for
that marker. It verifies the pinned Bazelisk binary selecting `.bazelversion`.
The verified locked-input identity is passed to Bazel action/repository
environments and exposed as a declared native toolchain input so OS-input changes
do not reuse old actions. A native Bazel JVM startup setting also carries that
identity, forcing a daemon restart when the runtime changes. This prevents a
long-lived server from executing new actions in an older FHS namespace.
The native identity repository independently checks its environment value against
the marker. `PULSEBEAM_HOST_ENV` is reserved: supported builds/tests use `./bazel`
without overriding it via action, repository or test environment options.
Deliberately suppressing the repository rc or replacing its environments/toolchains
is a native Bazel escape hatch, not a supported runtime-identity guarantee.

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
Xcode/SDK provisioning; the Linux FHS runtime is not a macOS environment.

Missing libraries or failed pinned downloads are errors, not reasons to skip
acceptance. Browsers need working user namespaces or their documented headless
sandbox mode, a writable temporary directory, and available loopback ports.
Privileged perf/network diagnostics and a Docker-compatible daemon are optional
and not prerequisites for the complete test gate. The image loader consumes a
Bazel-built artifact; the provisioned client does not build images.

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

OS identity, declared inputs, toolchains and action arguments remain part of the
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
fast owner tests. `//:test` adds committed slow simulations, both RTC browser
matrices, the local RFC 8888 Go peer, and Web/React browser contracts. Ignored
exploratory cases and advisory seed search retain their separate semantics.
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
