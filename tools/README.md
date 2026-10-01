# Build integrations and pin authority

Maintained upstream rules own compilation, dependency resolution, generation
interfaces, language-server project discovery and standard artifact operations.
Repository scripts are bounded adapters for uncovered contracts, not a general
build framework or an alternative package compilation pipeline.

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
| Linux OS inputs and mold | `tools/host/flake.nix`, `tools/host/flake.lock` | Narrow Nixpkgs FHS runtime; no application build graph |
| Go SDK/dependencies | RTC peer `go.mod`/`go.sum`; server SDK `sdks/go/go.mod` uses the same minimum Go version and standard library only | rules_go/gazelle module extensions |
| Python server SDK dependencies | `sdks/python/pyproject.toml` (public requirements), `requirements.in`/hashed `requirements.lock` (repository tools and concrete test resolution) | rules_python pip hub |
| Browser binaries/drivers | RTC `browser/browser-matrix.json` | Browser extension and RTC version probes |
| Runtime image | Immutable cc-debian13 manifest digest in `MODULE.bazel` | rules_oci image |
| Release identity/version/layout | Server Cargo manifest and `release/distribution.toml` | Archives, installer, plans and publication |

After editing authoritative Rust/generator inputs, explicitly run
`./bazel run --lockfile_mode=update //tools:update-rust-pins`, then
`./bazel mod deps --lockfile_mode=update`, and review derived files. Never edit
`rust-pins.MODULE.bazel`, `rust-metadata.bzl`, `wasm-deps.bzl` or
`tools/rust/*/Cargo.toml` by hand. Cargo/pnpm lock maintenance remains separate
from supported application compilation; CI does not install parallel host tools.

## Adopted upstream integrations

- `rules_rust`: Rust targets, build scripts, Cargo metadata/lints, Clippy,
  rustfmt, crate-universe and editor-neutral rust-analyzer setup/discovery/flycheck.
- `rules_rust_wasm_bindgen`: matching glue generation and WASM toolchain use.
- `toolchains_llvm`, `rules_cc`, `rules_foreign_cc`, `nasm`: native compiler,
  CMake/Ninja and codec assembler inputs. The LLVM integration consumes the
  locked OS runtime's unwrapped mold executable; its current linker API accepts
  a path, not a provisioned-artifact label.
- `protobuf`: generated Rust protobuf input through the pinned code generator.
- `rules_js`, `rules_nodejs`, `bazel_lib`: frozen pnpm translation, first-party
  package links, JS execution/tests, directory outputs and declared artifact views.
- `rules_go` and `gazelle`: pinned local RFC 8888 peer and standard-library server SDK, not a hosted Go service.
- `rules_python`: compatible provisioned runtimes for bounded glue and the server SDK's native crypto dependency. The Python SDK invokes the pinned setuptools backend against its authoritative pyproject for wheel/sdist packaging. Its consumer uses the standard installer library to test the wheel.
- `rules_pkg` and `rules_oci`: binary layer packaging, immutable non-root base,
  linux/amd64 image and optional daemon loading.

## Concrete repository-owned gaps

- `host-env.bzl` exposes the locked OS environment's immutable identity as a
  declared native compile/link input, independently matching it against the watched
  immutable runtime marker. The launcher also passes it through action/repository
  environments; the identity is reserved for supported builds/tests. The OS-only flake is separate from application
  sources, so the verified flake/lock/architecture identity does not depend on
  worktree location.
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
  would not cover. The locked OS runtime supplies shared libraries; kernel
  namespace/sandbox support remains an explicit host prerequisite.
- `run-simulations.py` preserves nextest-style per-case process isolation and
  slow namespace selection with libtest, including seed replay and advisory
  windows. It runs actual Bazel-built test binaries, not Cargo commands.
- `ide.py` materializes ignored generated SDK/framework assets and locked editor
  dependency trees. Rust crate/configuration information comes exclusively from
  upstream rules_rust discovery. No bespoke language server/project graph exists.
- `fix.py` explicitly applies provisioned formatters to source-workspace paths;
  normal hermetic formatter tests operate on declared inputs. `check-js-format.py`
  dereferences only declared snapshots inside the test's temporary directory:
  Prettier otherwise rejects explicit Bazel runfiles symlinks and silently omits
  globbed symlink inputs. Every original check pattern remains required.
* `npm-links.bzl` uses the upstream package linker for Meet's transitive local
  Web SDK dependency. Meet's independent lockfile does not encode that edge
  through its linked React package; the bridge references the existing package
  output, with no duplicate version pin or application resolver alias.
- `rules_pkg` owns release archives. `release/package.py` adds hashes and assembly
  metadata; `global.py` and the installer template preserve package-scoped tags
  and CARGO_HOME installation. Updates reuse the explicitly selected release's
  installer, without a separate updater or receipt. These actions consume
  already-built artifacts. Publication is credentialed workflow glue, never a
  build action.
- Static example serving and scoreboard regeneration are bounded utilities for
  their concrete outputs. Source metadata, scripts and fixtures are declared.

The integration inventory describes ownership, not passing acceptance evidence.
See the [migration evidence record](bazel-acceptance.md) for actual commands,
results and explicit exclusions.
