# Editor-neutral language servers

Bazel supplies the project/toolchain information. Neovim is the reference client;
other standard LSP clients can use the same executables/settings. Cargo/pnpm
metadata supports editing and dependency maintenance, not an independent native
compilation workflow.

## Setup and refresh

From the checkout root, after the [OS baseline](build.md#host-prerequisites):

```sh
export PATH="$PWD:$PATH"
./bazel run //:ide
./bazel run //:rust_ide -- neovim
```

The PATH entry selects the repository's checksum-verified launcher when the
upstream integration invokes `bazel`; it does not install a separate host tool.
Launch the editor with that same environment.

`ide` builds real SDK/native/generated outputs and Meet framework types, copies
only ignored editor assets into their familiar source-package locations, then
uses Bazel's pinned pnpm to install frozen, script-disabled editor dependency
trees. It does not compile through Cargo, execute package lifecycle scripts, or
create a second dependency graph. Normal application actions do not consume
these editor copies.

The Rust setup is maintained by `rules_rust`. It installs ignored discovery and
flycheck launchers under `.rules_rust_analyzer` and prints the Neovim configuration
for the provisioned rust-analyzer and matching proc-macro server. The installed
server launcher supplies declared Cargo, Rustc and Rustfmt paths, rather than
ambient host tools. The printed configuration is authoritative, not an assumed
launcher filename.
Use the printed settings unchanged rather than generating a Cargo-only project.
For another client:

```sh
./bazel run //:rust_ide -- print
```

After a manifest/lock, BUILD, toolchain, generator or generated-input change,
rerun both refresh commands. Rust discovery watches BUILD/MODULE changes;
refresh rebuilds generated code and reinstalls locked JS type dependencies.
Restart affected LSP clients after toolchain changes. If the checkout moves,
regenerate settings because the upstream snippet contains absolute tool paths.

## Rust

Use the Neovim snippet printed by `//:rust_ide` with `nvim-lspconfig`, or translate
its `cmd` and `settings` to Neovim's native `vim.lsp.config` API. Keep the matching
proc-macro server and saved-file flycheck configuration. The default formatter
uses the declared Rustfmt executable and the crate edition from analysis; do not
add a formatter command that duplicates the edition. This covers derives/attributes
and Bazel-generated protobuf and UniFFI inputs, not merely hand-authored Rust.

The upstream discovery/flycheck implementation derives dependency edges,
features, cfgs, proc-macro artifacts, build-script environment and generated
sources from the configured Bazel graph. Normal/native, simulation and WASM
proof targets retain their distinct configurations. Do not maintain a parallel
list of crates or invented native/WASM Cargo build commands.

Compiler-backed diagnostics are available through the upstream flycheck
integration, scoped to the edited target rather than the full acceptance suite.
Keep `check.workspace = false`: whole-workspace restarts cannot supply the saved
file and would cancel its check when initial indexing finishes. This does not
disable saved-file checks or whole-workspace reference discovery.
Its optional Clippy integration is enabled by:

```sh
./bazel run //:rust_ide -- --clippy neovim
```

By default discovery focuses on the package/dependency closure to bound memory.
Open dependent package files when navigating across packages. For whole-workspace
reference searches, use the supported upstream option:

```sh
./bazel run //:rust_ide -- --no-per-package-workspaces neovim
```

This loads substantially more Rust dependency state and needs correspondingly
more memory. Return to bounded discovery with `--per-package-workspaces`.

## TypeScript and JavaScript

The compatible language server and TypeScript runtime are pinned in the Web
package's manifest/lockfile and run through the same Node toolchain:

```sh
./bazel run //agents/pulsebeam-agent-web:lsp
```

The target already supplies `--stdio`. A Neovim 0.11+ configuration using native LSP APIs is:

```lua
local root = vim.fs.root(0, { "MODULE.bazel" })
vim.lsp.config("pulsebeam_ts", {
  cmd = { root .. "/bazel", "run", "//agents/pulsebeam-agent-web:lsp" },
  root_dir = root,
  filetypes = { "typescript", "typescriptreact", "javascript", "javascriptreact" },
  init_options = {
    hostInfo = "neovim",
    tsserver = { path = root .. "/agents/pulsebeam-agent-web/node_modules/typescript/lib/tsserver.js" },
  },
})
vim.lsp.enable("pulsebeam_ts")
```

Normal tsconfig/package discovery resolves generated Web bindings, the local
Web and React packages, dependency declarations, Meet `@/*` aliases, and generated
`.next` framework types after refresh. Completion, hover, definition/references,
type diagnostics and TypeScript formatting use that actual dependency tree.
`./bazel run //:fix` applies the repository's provisioned Rust/Prettier style
explicitly; ordinary saves/tests do not rewrite tracked generated code.

## Acceptance checklist

Exercise completion, hover, definition and references in representative native,
proc-macro, generated-protobuf, generated-UniFFI, React and Meet files. Check a
cross-package core type reference and a generated Web binding reference. Valid
code must not report missing dependencies/generated files. Introduce a small
type error, observe the compiler/TypeScript diagnostic, repair it and confirm it
clears. Exercise Rust/TS formatting and refresh after a representative generated
input/dependency change. Installing a client plugin alone is not acceptance
proof; recorded evidence belongs with the migration's verification results.
