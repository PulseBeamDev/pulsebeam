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

## Shared local caches

`.bazelrc` enables Bazel's native fetched-repository contents cache at
`~/.cache/pulsebeam/bazel/repo-contents-cache`. Reproducible external repositories
reuse their extracted/generated directories across worktrees. Pinned Rust archive
checksums are derived from the official release manifest by `//tools:update-rust-pins`,
so tool repositories can reuse both downloaded archives and fetched contents.
The `rules_go` patch retains resolved SDK URLs/hashes in the lockfile and marks
pinned, checksummed, explicit-platform SDK repositories reproducible.

Enabling contents caching does not seed repositories already present in an
existing output tree. A successful run that fetches them populates the shared
cache; subsequent equivalent worktrees reuse those directories.
The separate download cache retains Bazel's default location under
`<output_user_root>/cache/repos/v1`. `--repo_contents_cache=` explicitly disables
contents reuse; `--repository_cache=` disables the download cache.

`.bazelrc` also enables Bazel's native disk cache at
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
`--disk_cache=` explicitly disables action caching for diagnostics.

Native input-tree memoization avoids repeated Merkle-tree construction within a
command, including when using the disk cache. Its default 1,000-entry bound is
not a guarantee of retained useful entries: soft references can be reclaimed.
Tests defer runfiles symlink creation until local execution needs it; ordinary
build commands retain their default behavior. Filesystem watching reduces scans
for locally edited checkouts. Use `--nowatchfs` for network filesystems edited
from another machine, where native watching cannot reliably detect changes.

A fresh worktree still starts its own Bazel server, analyzes the graph, hashes
declared inputs, and prepares cached outputs, even when all build and test
results are cached. Repository contents caching does not share in-memory
analysis state. A “fetching” progress message need not represent a network
download. To measure
first-run overhead separately from a warmed server, capture `--profile=<path>`
and `--build_event_json_file=<path>` in a fresh worktree, then repeat the same
command there. The build-event runner counts and cached test summaries distinguish
execution from disk-cache hits and internal materialization.

## Entrypoints

```sh
./bazel build //:server //:cli //:native //:web //:react //:meet //:docs
./bazel test //:fast --test_output=errors
./bazel test //:test --test_output=errors
```

| Gate | Purpose |
| --- | --- |
| `//:fast` | Focused Rust unit/integration and fast isolated simulation behavior, source auth/Go conformance, source formatting and inexpensive tool contracts. No distribution assembly, browser fixtures, Meet export or docs site. |
| `//:check` | Broad compiler, architectural, generated-interface, package-consumer and artifact contracts. This is not the cheap feedback loop. |
| `//:slow` | Committed slow simulations, controlled native RTC interoperability, and existing Web/React/Meet browser acceptance. |
| `//:test` | Complete non-privileged acceptance: the union of all three gates, including Rust-pin validation. |

Owner targets are authoritative for selection and dependencies. An owner's
`:fast` may include package contracts and need not have the root cheap gate's
cost. Ignored exploratory cases, advisory seed sweeps and privileged diagnostics
remain outside ordinary acceptance. The sole documented gate exclusion is the
[native DTLS reconnect case](https://github.com/PulseBeamDev/pulsebeam/blob/main/crates/pulsebeam-simulator/docs/native-dtls-exception.md):
it is reported as skipped, not passed, and remains explicitly runnable.

Targeted selection uses the owning target and its harness:

```sh
./bazel test //crates/pulsebeam-routing:unit_tests --test_arg=--exact --test_arg=tests::example
./bazel test //crates/pulsebeam-simulator:fast --test_arg=--filter --test_arg=tests::bwe
```

Replace the example Rust name with a real test name. Simulation libraries retain
optimization level 2, debug assertions and overflow checks. Named `dev`,
`release`, and `profile` configurations preserve the relevant panic/assertion
settings; use those build configurations for runtime binaries, not abort-panic
settings for libtest.

## Testing contract

### Keep the QA loop fast

Nearly all behavioral coverage must run very fast. Use deterministic end-to-end
simulation of real production logic for system behavior, with virtual time and
execution faster than real time. Broad coverage is a reason to improve the
simulator, not to replace the system with mocks or retreat to isolated unit
proof. Keep narrow local behavior in direct unit tests.

Every QA change must maintain or improve feedback speed, reliability, meaningful
coverage, and maintenance cost. Reuse fixtures and scenarios, remove redundant
work, and optimize the harness before adding more machinery. Slow simulations
are an infrastructure problem to fix; relabeling them `slow`, adding workers,
weakening assertions, or cutting meaningful cases is not that fix.

Treat browser automation as expensive and flakiness-prone. Keep it exceptional,
narrow, and mostly smoke tests for browser-only behavior. Put behavioral and
property matrices in fast deterministic tests, not browser sessions. Any added
real-boundary cost must name the unique regression that simulation cannot
faithfully detect and use the smallest smoke that detects it.

For workload changes, report uncached execution cost before and after, separately
from build/cache time, and explain preserved fidelity. New cost must earn its
place; test counts are not an achievement. These are design and review duties,
not flaky wall-clock assertions. Existing required gates still apply until an
explicit migration removes their obsolete proof.

### Client QA boundaries

| Behavior | Authoritative proof | Fidelity limit |
| --- | --- | --- |
| System policy: loss, recovery, ordering, history and cross-shard behavior | Deterministic native/SFU simulation | Component models do not replace the integrated system oracles. |
| Supported emitted JS/WASM ABI and lifecycle paths | Web owner's pinned-Node artifact tests | Instantiate real emitted WASM and call generated exports. Controlled sender completions exercise ownership and cancellation, not negotiated RTC, codecs or browser compatibility. Node is a QA host, not a supported browser substitute for applications. |
| React lifecycle, capture and playback policy; Meet join, identity, topics, credentials and Leave | Owner component tests in pinned Node with real React and Happy DOM | Render production hooks and component trees with effects, subscriptions, events and unmounts. Only external runtime snapshots, capture, geometry and playback outcomes are modeled. No real layout, decoding, autoplay security or connected-media claim. Credential observation covers exercised flows, including transient effects, not every possible application path. |
| Irreducible Web-platform glue | Required Web browser owner | Shipped SDK with a real local SFU and fresh decoded media, adapter reconnect, real-track generated handoff, element/observer/device-pixel plumbing and isolated early/late autoplay premises. |

Put a new behavioral test in the narrowest faithful owner above. Admit a browser
assertion only when it names a unique **PulseBeam platform regression** that
narrower proof cannot catch; testing the browser's general API conformance is not
the goal. Keep policy and lifetime matrices out of browser sessions.

Root `//:fast` includes cheap Web facade/decision and React/Meet component tests
without distribution assembly, Meet export or browser provisioning. Artifact,
package-consumer and browser proof remain required in `//:test`, not optional
because they cost more. Executable owner targets define exact membership. The
Web browser owner needs pinned tooling, built Web artifacts and owned local
SFU/assets, but no Meet export or React acceptance fixture.

### Choose proof that earns its cost

Choose the smallest deterministic, faithful proof in this default order: **unit,
simulation, simulation property, integration, end-to-end**. Scope and cost are
separate: end-to-end application behavior can run in a fast simulation without
a browser or wall-clock waiting. Keep genuinely irreducible expensive proof in
an honest owner route without making it optional in complete acceptance; that
classification does not excuse avoidable cost.

Retain a higher-tier check only for a distinct failure that needs that boundary.
Similar scenarios across language, wire or platform boundaries are not
automatically redundant. Each test addition, relocation or deletion needs a
meaningful regression argument. New tests must earn their maintenance cost;
migrations must preserve unique proof, complete the cutover and remove the
obsolete path.

Consolidate compatible assertions around shared setup and reports. Assert direct
resulting state and externally visible effects, not source/bundle spelling or
private call ordering. Avoid fixed sleeps, post-run rejection that hides failed
liveness, and silent conditional non-exercise. Generated failures must be
reproducible with deterministic seeds and retained regression inputs/artifacts.
Provisioning failures are errors, not passing skips; exceptions must be narrow,
explicit and attributable.

Correctness tests assert semantic, resource and protocol bounds. Host runtime,
throughput and scale measurement belong in benchmarks, not flaky wall-clock
speed assertions. Counts and coverage quotas do not enforce proof quality.

### Preserve isolation and attributable evidence

Let Bazel schedule independent owners using declared resources. Simulator test
actions reserve one CPU per child from the same owner-local worker budget;
standalone replay honors `--jobs`. Each simulator case still gets a separate
process and one libtest thread because virtual-clock/RNG hooks are process-wide.
Never execute its manual raw libtest binary from an aggregate. Controlled native
worlds and browser ports/sessions retain their owner-local serialization.

Normal simulator output identifies case, seed selection (`committed` means the
case's authored defaults), outcome, elapsed host time and exact replay command.
Full failed-case stdout/stderr stays in Bazel's undeclared test outputs; the
runner prints its path. Replay retains failure logs in a temporary directory.
Use `--show-output` for grouped diagnostic traces; `RUST_LOG` still selects
tracing detail. Scoreboard regeneration explicitly consumes grouped metrics.
See the [simulator guide](https://github.com/PulseBeamDev/pulsebeam/blob/main/crates/pulsebeam-simulator/README.md)
for seed, fidelity and exception details.

### Iterate narrowly, accept completely

Run the exact affected proof, then the nearest owner's fast gate while iterating.
Use an exact slow test only when debugging that boundary, not an aggregate slow
run per slice. Freeze the integrated candidate and run `./bazel test //:test`
once for final acceptance, including all existing browser boundaries.

Report actual execution versus cached results and any resource limitations.
Reuse valid evidence unless it is missing, stale, contradictory or needed to
reproduce a finding. After a repair, rerun affected owners, including every
affected browser boundary for shared changes, and reuse unaffected passing slow
evidence rather than repeating complete acceptance. Never change the tested
candidate while verification is running.

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

`fix` applies unambiguous machine-applicable Clippy suggestions, then formats
source with provisioned Rust/JS tools. Its Clippy pass requires compiling Rust
code: repair compilation errors explicitly before retrying `fix`. Normal
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
