# Build and development

Bazel is the supported compilation, generation, and test graph. Run commands
from the repository root. Normal development and full browser acceptance support
Linux x86_64. Linux aarch64 runners support release binary assembly only.

## Host prerequisites

The reference baseline is Ubuntu 24.04, a writable checkout/cache, and ordinary
non-root execution. Install OS utilities, system headers, and runtime libraries:

```sh
sudo apt-get update
sudo apt-get install -y --no-install-recommends ca-certificates curl git tar xz-utils unzip patch libc6-dev linux-libc-dev libstdc++-13-dev zlib1g-dev
sudo apt-get install -y --no-install-recommends libasound2t64 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdbus-1-3 libdrm2 libgbm1 libglib2.0-0t64 libgtk-3-0t64 libnspr4 libnss3 libpango-1.0-0 libx11-6 libx11-xcb1 libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxrandr2 libxrender1 libxt6 libxtst6
```

Git is a source-fetch utility required by Bazel's upstream Git repository rules,
not a language build workflow. The second command supplies browser OS libraries,
not browser executables.
Bazel provisions versioned compilers, Rust libraries, Node/pnpm, Python, Go,
protobuf, native build tools, generators, browsers and drivers. No host language
compiler, package preparation sequence, development container, or language/browser
cache is required. Initial dependency downloads need network access and CA trust.
Tests use local fixtures and loopback services, not hosted application services.

Missing libraries or failed pinned downloads are errors, not reasons to skip
acceptance. Browsers need working user namespaces or their documented headless
sandbox mode, a writable temporary directory, and available loopback ports.
Privileged perf/network diagnostics and a Docker/Podman daemon are optional and
not prerequisites for the complete test gate.

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
