# Build integrations and pin authority

Maintained upstream rules own compilation, dependency resolution, generation
interfaces, language-server project discovery and standard artifact operations.
Repository scripts are bounded adapters for uncovered contracts, not a general
build framework or an alternative package compilation pipeline.

`.bazelrc` shuts down Bazel servers after five minutes of inactivity and enables
idle shutdown under low system memory. This limits memory retained across
worktrees while preserving server reuse during active development. Each worktree
needs the updated configuration; disk caches remain available after shutdown.
The launcher keeps disposable output state inside each checkout's ignored
`.bazel/` directory, so deleting the worktree deletes those outputs. Shared
caches stay outside the checkout. See [cache ownership and legacy output cleanup](../docs/build.md#shared-local-caches).

## Authoritative sources

| Input | Authority | Derived consumers |
| --- | --- | --- |
| Bazel | `.bazelversion`; launcher Bazelisk version/checksums in `bazel` | Local and CI launch the same executable |
| Rust channel/components/targets | `rust-toolchain.toml` | `tools/rust-pins.MODULE.bazel` toolchains/host tools |
| Rust dependencies/features/lints, including the server SDK, shared auth primitives and conformance proof | Cargo manifests and `Cargo.lock` | Native/browser crate-universe graphs, `rust-metadata.bzl` and `wasm-deps.bzl` |
| WASM glue/runtime and UniFFI | Cargo lock/manifests | Build-tool universe, generated bindings and proof runtime |
| UBRN generator | `tools/generators.toml` | Derived generator repository |
| JS dependencies | Each owner package manifest and frozen pnpm lock | rules_js npm repositories and editor installs |
| Node, pnpm, Python, LLVM, CMake/Ninja and upstream rules | `MODULE.bazel` | All configured actions and CI |
| Linux OS inputs and mold | `tools/host/flake.nix`, `tools/host/flake.lock` | Declared native SDK/mold and patched browser runtime inputs; no application graph |
| Go server SDK | `server/go/go.mod`, standard library only | Go compilation toolchain and consumer-owned verification share the derived SDK |
| Python server SDK dependencies | `server/python/pyproject.toml` (public requirements), `requirements.in`/hashed `requirements.lock` (repository tools and concrete test resolution) | rules_python pip hub |
| Browser binaries/drivers | `tools/browser-matrix.json` | Browser extension and consumer-owned Web/React coverage |
| Native WebRTC inputs | Package-owned pins, patches and GN configuration in `crates/pulsebeam-libwebrtc-sys` | Local Rust sys target, GN native build and controlled RTC simulation |
| Runtime image | Immutable cc-debian13 manifest digest in `MODULE.bazel` | rules_oci image |
| Release identity/version/layout | Server Cargo manifest and `release/distribution.toml` | Archives, installer, plans and publication |

After editing authoritative Rust/generator inputs, explicitly run
`./bazel run --lockfile_mode=update //tools:update-rust-pins`, then
`./bazel mod deps --lockfile_mode=update`, and review derived files. Never edit
`rust-pins.MODULE.bazel`, `rust-metadata.bzl`, `wasm-deps.bzl` or
`tools/rust/*/Cargo.toml` or `tools/patches/str0m-rtc-visibility.patch` by hand.
Cargo/pnpm lock maintenance remains separate from supported application
compilation; CI does not install parallel host tools.

### Build the local WebRTC bindings

Native SDK and controlled RTC builds consume
[`crates/pulsebeam-libwebrtc-sys`](../crates/pulsebeam-libwebrtc-sys/README.md).
Normal Bazel commands acquire pinned WebRTC/DEPS sources and tools through the
package repository rule, then compile WebRTC, CXX and the adapters with GN/Ninja
in a declared action. No candidate archives, preparation command, archive
environment variables or external sys worktree are required.

```sh
./bazel test //crates/pulsebeam-libwebrtc-sys:fast \
  //crates/pulsebeam-rtc/simulation:h264_fixture \
  //crates/pulsebeam-rtc/simulation:packet_bridge
```

Linux x86_64 core is the default. The package-owned `flavor` setting selects
native consistently across the configured graph, including Rust feature cfgs.
The native package owns desktop libraries from its pinned target sysroot, not
the global application SDK. Both configurations retain software codecs and
protobuf events. See the package guide for native and focused ASan commands.

Whole-native-build caching is intentional. Ordinary Rust edits reuse the native
result; bridge, adapter, patch, pin, toolchain or GN configuration edits may
rebuild the whole native graph. Acquisition may use the network; native compile
and link actions may not. Update native pins and behavior patches in the package,
then run both configurations and the affected consumer proofs. Runtime limits
remain documented by the native owner's
[support and verification scope](../agents/pulsebeam-agent-native/QUALIFICATION.md).

### Update RTC's upstream str0m

1. Change `str0m` and `is` in `crates/pulsebeam-rtc/Cargo.toml` to the same
   upstream Git revision and refresh the root `Cargo.lock` through Cargo dependency
   maintenance. Preserve RTC's feature selection.
2. Run the pin-refresh and module-lock commands above. Pin refresh selects the
   locked RTC package from Cargo metadata and derives the visibility patch from
   its resolved source without modifying Cargo's checkout. Review the generated
   patch, RTC universe manifest/lock, annotations and module lock.
3. Run the affected [verification gates](../docs/build.md#testing-contract) and
   refresh [editor assets](../docs/ide.md#setup-and-refresh).

Bazel applies the declared patch only to RTC's str0m universe. Normal builds,
checks and rules_rust IDE discovery consume that patched source without
regenerating it. No fork-publishing script, sibling checkout, GitHub write access,
manual patch rebase or separate visibility-generation command is required.
The workspace `feat/bwe-6` str0m fork and its `str0m_native` universe remain
for other consumers; the universe name does not imply native-agent execution.
The native agent's normal and simulation-feature libraries use libwebrtc, not
that fork.

The transform exposes root `crypto`, `dtls`, `change`, `format`, `sdp` and `rtp_`
modules. It widens line-leading `pub(crate)` and bare child-module declarations
only in `src/{crypto,change,format,sdp,rtp}/**/*.rs` and `src/dtls.rs`. Missing
required files/trees or missing/ambiguous root declarations fail pin refresh. Upstream internals are not a stable API:
future source changes may require adapting the transform or RTC consumers, and
compilation failures must be repaired rather than bypassing exposure. Standalone
Cargo compilation is not a supported patched build path.

## Adopted upstream integrations

- `rules_rust`: Rust targets, build scripts, Cargo metadata/lints, Clippy,
  rustfmt, crate-universe and editor-neutral rust-analyzer setup/discovery/flycheck.
- `rules_rust_wasm_bindgen`: matching glue generation and WASM toolchain use.
- `toolchains_llvm`, `rules_cc`, `rules_foreign_cc`, `nasm`: native compiler,
  CMake/Ninja and codec assembler inputs. The LLVM integration consumes the
  declared Nix SDK and unwrapped mold executable; its current linker API accepts
  a path, not a provisioned-artifact label.
- `rules_nixpkgs_core`: imports concrete OS packages from the existing flake lock.
  Nixpkgs `buildEnv` owns SDK projection and `autoPatchelfHook` owns browser ELF
  interpreter/runtime-library integration on the normal host.
- `protobuf`: generated Rust protobuf input through the pinned code generator.
- `rules_js`, `rules_nodejs`, `bazel_lib`: frozen pnpm translation, first-party
  package links, JS execution/tests, directory outputs and declared artifact views.
- The standard-library Go server SDK retains its consumer-owned verification; RTC no longer uses a Go peer.
- `rules_python`: compatible provisioned runtimes for bounded glue and the server SDK's native crypto dependency. The Python SDK invokes the pinned setuptools backend against its authoritative pyproject for wheel/sdist packaging. Its consumer uses the standard installer library to test the wheel.
- `rules_pkg` and `rules_oci`: binary layer packaging, immutable non-root base,
  linux/amd64 image and optional daemon loading.

## Concrete repository-owned gaps

- `os.bzl` connects the existing flake lock to upstream Nix repository rules:
  their module-extension tags do not expose the lock-file import API.
  `native.bzl` translates the imported mold path to upstream LLVM's path-only
  linker API and declares the artifact. SDK headers/libraries use its native
  sysroot label interface. These adapters do not install tools, resolve versions,
  or orchestrate application builds.
- `update-rust-pins.py` projects existing workspace/manifests into the external
  crate-universe first-party override boundaries and tool-only universes. A
  browser-only universe is derived from the Web/Core/protobuf normal dependencies,
  retaining root-lock package identities while preventing native workspace Tokio
  networking features from entering WASM. The generated selector changes only
  those libraries' WASM dependency closures. There is no manually maintained
  second workspace/dependency list.
- `ubrn-codegen.py` and `native-codegen.py` create declared offline Cargo metadata
  snapshots because UniFFI/UBRN library-mode namespace discovery still queries
  Cargo. Cargo is used for metadata only, never application compilation. A narrow
  UBRN patch limits discovery to workspace metadata; Swagger's patch makes its
  generated embedded path relocatable in Bazel sandboxes.
- `browsers.bzl` preserves the existing matrix's checked URLs, hashes, archive
  structure and Firefox ESR contract, which a generic Chrome-only toolchain
  would not cover. It passes matrix values to a separately locked Nixpkgs browser
  derivation. Upstream autoPatchelfHook handles all ELF helpers and shared
  libraries without a process-wide library override or FHS runtime. Kernel
  namespace/sandbox support remains an explicit host prerequisite.
- `run-simulations.py` preserves nextest-style per-case process isolation and
  slow namespace selection with libtest, including seed replay and advisory
  windows. It runs actual Bazel-built test binaries, not Cargo commands.
  Its owned bundles retain host timing and checksummed failure artifacts,
  including concrete history inputs. Evidence-retention errors remain separate
  from the child failure phase. See the [evidence and replay contract](../crates/pulsebeam-simulator/docs/simulation.md#retain-and-replay-generated-failures).
  These diagnostics do not introduce SDK observations or private-state oracles.
- Two bounded `rules_rust` patches keep its editor setup/discovery/flycheck
  ownership: setup supplies declared tool paths through upstream launchers,
  forwards tool runtime closures, and emits saved-file checks/default formatting;
  flycheck's source-owner fallback excludes non-compiling filegroups. Rust tools
  share the `rust-toolchain.toml` version, without separate analyzer pairing.
- `ide.py` materializes ignored generated SDK/framework assets and locked editor
  dependency trees. Rust crate/configuration information comes exclusively from
  upstream rules_rust discovery. No bespoke language server/project graph exists.
- `fix.py` explicitly applies provisioned formatters to source-workspace paths;
  normal hermetic formatter tests operate on declared inputs. `check-js-format.py`
  dereferences only declared snapshots inside the test's temporary directory:
  Prettier otherwise rejects explicit Bazel runfiles symlinks and silently omits
  globbed symlink inputs. Every original check pattern remains required.
* `npm-links.bzl` uses the upstream package linker for the transitive local Web
  SDK dependency of Meet and the React consumer contract. Their independent
  lockfiles do not encode that edge through the linked React package; the bridge
  references the existing package output, with no duplicate version pin or
  application resolver alias.
- `rules_pkg` owns release archives. `release/package.py` adds hashes and assembly
  metadata; `global.py` and the installer template preserve package-scoped tags
  and CARGO_HOME installation. Updates reuse the explicitly selected release's
  installer, without a separate updater or receipt. These actions consume
  already-built artifacts. Publication is credentialed workflow glue, never a
  build action.
- Static example serving and scoreboard regeneration are bounded utilities for
  their concrete outputs. Source metadata, scripts and fixtures are declared.

The integration inventory describes ownership, not passing acceptance evidence.
See [build and development](../docs/build.md) for the supported provisioning path.
Acceptance records must distinguish passing execution from pending or qualified
boundaries; source wiring alone is not runtime evidence.
