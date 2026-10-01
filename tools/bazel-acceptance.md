# Bazel migration acceptance

## Outcome and explicit exclusions

Bazel owns the Rust/native/WASM, Web/React, Meet, docs, IDE setup, distribution
assembly and automation graph. No production GitHub release, GHCR push or Pages
deployment was performed.

Native aarch64 release execution was explicitly deferred by the human for this
handoff. ARM runner and packaging wiring was audited, not runtime-validated. The
known native DTLS reconnect simulation is the sole authorized merge-gate
exclusion, documented in
[the exception record](../crates/pulsebeam-simulator/docs/native-dtls-exception.md).
Neither deferred ARM execution nor that excluded case is reported as passed.

## Reference environment and aggregate gate

The reference environment was rootless Ubuntu 24.04 on Linux x86_64, with the
live owner checkout mounted directly and only the OS packages documented in
[build prerequisites](../docs/build.md#host-prerequisites), installed without
recommendations. This container was an acceptance harness, not a required
workflow or development environment.

Before bootstrap, the probe confirmed no ambient Cargo, Rust, Rustup, Node,
npm, pnpm, Go, C/C++ compiler, Python, CMake, Ninja, protoc, browser driver or
language/browser cache. Ignored editor, dependency and build mirrors were moved
outside the checkout. The dedicated disk-backed cache started empty; no host
build caches were imported.

Initial attempts found a missing documented Git source-fetch prerequisite,
a tmpfs cache quota limit, a Python launcher requiring host Python, and RTC
fixture clock-origin drift under slow construction. These were repaired rather
than adding language tools or weakening acceptance. After those repairs,
commands resumed with the cache populated solely by the same isolated acceptance
runs. This was not a second empty-cache rerun of the final source revision.

Passing commands:

```sh
./bazel version
./bazel build //:server //:cli //:native //:web //:react //:meet //:docs --jobs=8
./bazel test //:test --test_output=errors --jobs=8
```

The stable 73-target complete gate passed, then the newly added scoreboard
contract and updated 74-target aggregate passed. This includes static and
architectural checks, fast and slow committed simulations, RTC Chrome/Firefox,
the local RFC 8888 Go peer, Web/React browser contracts and artifact contracts.
The final aggregate reuses unaffected passing Bazel test results; changed owners
were rerun. Helper imports no longer create bytecode in the source checkout.
Tracked diffs and all untracked migration-source hashes were verified unchanged
by ordinary builds/tests and representative scoreboard generation.

## Runtime, IDE and distribution evidence

- Fast owner gates and targeted selection passed during iteration.
- Real Neovim 0.12 LSP requests using printed upstream Rust settings and the
  provisioned TypeScript server passed completion, hover, definitions,
  references and valid-code diagnostics for Rust identity/proc macros,
  generated protobuf, generated Web bindings, React and Meet. Procedural macro
  expansion and Rust/TS formatting passed. An unsaved TS error was diagnosed
  and cleared after repair. Refresh after generated-input changes passed again.
- Server, Web example, Meet and VitePress dev entrypoints passed argument
  forwarding. Server metrics returned 200, Web WASM loaded as application/wasm,
  Meet rendered, and pinned Chrome rendered docs without application errors.
- `./bazel build --config=profile //:server --jobs=8` passed with retained
  debug information, frame pointers and upstream thin-LTO handling.
- `./bazel build --config=release //release:all --jobs=8` and the release
  contract passed for x86_64. Controlled fixtures validated archive, hash,
  installer and receipt-driven updater behavior without publication.
- OCI loading and rootless Podman argument smoke with `--network=none` passed.
  Metadata identifies amd64, non-root 65532:65532 and entrypoint `/app/pulsebeam`.
  Release publication waits for image validation rather than publishing first.
- Replay at seed 1 and an advisory two-seed window starting at 1 passed for
  `tests::connectivity::simulation_test`. Criterion `--test` exercised benchmark
  functions successfully; this is not a performance measurement or improvement
  claim.

## Incremental behavior and bounded scoreboard acceptance

Two unchanged builds selected server, Web, React, Meet and native bindings. The
no-op build executed no compilation or generation, only workspace status.
Reversible documentation probes in Rust identity, protobuf Participant, UniFFI
AgentConfig, Web RemoteCatalog and React useAgent rebuilt their affected
compilation, generation and export closures. Generated protobuf, native/Web
bindings and declarations contained the expected markers. All probes were
restored byte-for-byte; IDE refresh and real Neovim acceptance passed afterward.

Scoreboard now passes replay arguments explicitly: Bazel's run defaults are not
embedded in the executable when invoked as a subprocess. Plans execute serially
for deterministic metric/result grouping. Contract tests cover argument
forwarding, metric normalization, simulation-failure propagation and rejection
of missing records without writing a baseline.

A full serial regeneration was cancelled by the human pause after approximately
19 minutes and is not reported as passed. Actual generation with a representative
committed BWE plan passed and produced its PASS outcome and real link metrics:

```sh
./bazel run //:scoreboard -- \
  --runner-arg=--mode --runner-arg=fast \
  --runner-arg=--filter \
  --runner-arg=tests::bwe::subscriber_reaches_top_layer_on_fast_link_test \
  --output /tmp/pulsebeam-bwe-representative.txt
```

This exercises the real generator and parser without rerunning the already
accepted full simulation matrix or modifying tracked `bwe-baseline.txt`.

## Retained evidence identifiers

Verifier logs are session-local under `/tmp`; this document preserves their
scope and results, not a promise that those temporary files survive cleanup.

| Boundary | Evidence |
| --- | --- |
| Final representative scoreboard, 74-target aggregate and source integrity | `bf16da10-0aa1-468b-b82e-9f2f549978dc` |
| Stable OS-only builds and 73-target aggregate | First command of `a8c8e610-07d3-4fbd-97f7-f68549379a34`; its later scoreboard command failed before the argument repair |
| Scoreboard contract and artifact builds | First command of cancelled `d9750912-5e37-4ca2-9bd7-6e819c3bed04`; later full regeneration was cancelled |
| RTC fixture regression, formerly failing targets, format and Clippy | `349569d5-d6be-4d62-969f-8bb753c94a33`; regression failed before repair in `abb46981-7dd8-4e24-859c-d7c19e954ad1` |
| Helper contracts, replay, advisory sweep and Criterion test mode | `608f560a-3c5d-43ae-a5db-510c7a6d333c` |
| Neovim requests and refresh after generated changes | `04b94113-6700-4ef3-9cbb-a7d2b0a96d4b`, `bed07bbe-b765-4642-b437-551164528fe3`; `/tmp/pulsebeam-lsp-evidence.json` |
| Local dev/browser workflows | `eda28e8f-2603-4e7e-b6ea-4236a0d07abc`; `/tmp/pulsebeam-dev-evidence.json`, `/tmp/pulsebeam-docs-browser.json` |
| Profiling build | Profile command in `ad349a4d-7c0d-4304-9dae-f93143e137bd` passed before a separate OCI analysis failure was repaired |
| Release and image | `5a870ed7-b666-4adc-afc3-83b49f5a2d3b` |
| No-op reuse | `55e8f452-a3d8-43e7-aaa2-44f62ed45824` |
| Incremental markers | `6211daac-aad8-495b-adcb-7ef0e5ce9fe9`; `/tmp/pulsebeam-incremental-evidence.json` |

The earlier aggregate marked candidate drift is not used as passing aggregate
evidence. Final stable runs supersede it. No review convergence or publication
approval is asserted here.
