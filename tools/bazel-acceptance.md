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

During review, the human approved build/test ownership and legacy removal,
generated SDK/browser/IDE integration, and the distribution handoff with its
explicit native-aarch64 validation waiver. The human also approved canonical
package-scoped `pulsebeam-v<version>` tags as an intentional compatibility
exception to the old generic trigger and permissive tag parser. Unused
distribution manifest metadata was removed. The human subsequently
required mold, permitted narrow Nix OS provisioning, and authorized removing
separate updater/receipt compatibility in favor of Bazel-native archive assembly
and explicit versioned-installer updates. Earlier updater and host-baseline
evidence below describes the prior candidate, not acceptance of these renewed
changes. Renewed native, browser, image and shared-cache evidence is recorded
below. Complete coverage combines the aggregate with affected-owner reruns.
The human approved the qualified renewed handoff for tooling/tests, native
and image distribution, and the persistent shared local disk cache.
No approval here authorizes production publication.

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

## Renewed OS, distribution and shared-cache evidence

The renewed reference harness is rootless, non-root Linux x86_64 with the locked
OS-only Nix flake, not a required development container. Language/build/browser
tools remain Bazel-owned. This is not a renewed empty-cache complete-gate claim,
Fedora installation acceptance, native ARM execution or GitHub CI execution.

Receipt `de91f022-6982-435b-9f7a-3f27d7568f79` passed native Rust bootstrap linking,
launcher/plan/workflow checks, default archive/installer and ELF checks, profile
ELF checks, and release archive/installer and ELF checks. The only failed command
was `git diff --check` for a blank EOF line; its whitespace-only correction passed
`9c2451bf-3bb6-43b7-bf84-3026e1a3e8b3`. ELF checks require mold linkage, executable
startup, GNU symbol floors, and no Nix-store/build-directory runtime paths. The
new marker validation preserves identity-file bytes and action keys for legitimate
builds; renewed launcher/plan/workflow and diff checks passed
`a9ab4bfc-564b-4c39-978a-92e4a24add0f`.

The native disk cache is `~/.cache/pulsebeam/bazel/disk-cache`, with default
workspace-hashed output bases unchanged. Three fresh detached Git worktrees held
the same candidate. Their output state was expunged, not shared or copied. A
executed the routing unit test; B and C then ran concurrently, each restoring all
111 logged spawns from the persistent host-mounted cache, including the routing
test-binary `Rustc` action and two `TestRunner` spawns. Three distinct output bases
and daemon PIDs were recorded. A forged repository identity was rejected against
the actual immutable OS marker.

Receipt `3a5b4fe6-75e7-4eac-911b-95eb094cf7b2` completed all three test builds, but
postprocessing incorrectly expected an expanded cache path in build events and a
separate library action in the unit-test graph. Corrected retained-log analysis
passed `9ae8a487-459c-44e3-b200-d7dedc75c830`, without repeating the builds. The
summary and original execution/build-event logs are retained locally under
`/home/lukas/.cache/pulsebeam/cache-proof.9pltBF/evidence-resumed/`; that location
is evidence storage, not a development prerequisite. The physical cache remains
`/home/lukas/.cache/pulsebeam/bazel/disk-cache`, outside all proof worktrees.

A prior interrupted cache run is not used as completed concurrent-reuse evidence.
Its persistent cache and worktrees survived recovery; the resumed proof used
expunged output state and new retained logs. Scoped read-only distribution review
found no concrete release/automation defects. Deliberate native Bazel environment,
rc or toolchain overrides are unsupported escape hatches, not a cache guarantee.

## Final repaired-candidate acceptance

The renewed aggregate `36ac244c-e0d8-4278-a8a8-45cd57b01a16` ran `//:test` once:
75 of 77 targets passed, including static/architectural checks, fast/slow
simulations, RTC Firefox and the Go peer. RTC Chrome and Web/React browser tests
failed at Chrome startup. It is not reported as a passing aggregate invocation.
Direct startup and a complete loader trace identified missing expat, xkbcommon,
cairo and udev libraries. The existing locked OS runtime now supplies the
maintained Nixpkgs packages `expat`, `libxkbcommon`, `cairo` and `systemdLibs`.
No browser cases, assertions or tool pins changed.

Receipt `e99df649-5668-497a-a349-5d04c08206d2` passed Chrome startup and every
affected browser boundary, plus launcher/workflow and whitespace checks:

```sh
./bazel test //crates/pulsebeam-rtc:chrome //crates/pulsebeam-rtc:firefox \
  //crates/pulsebeam-rtc:rfc8888 //agents/pulsebeam-agent-web:browser \
  //tools:launcher_contract //tools:workflow_contract --test_output=errors --jobs=8
```

Receipt `069a1863-e9a1-4906-b296-1f49aa25b8a1` renewed native and distribution
checks against that repaired runtime:

```sh
./bazel test //release:contract //release:elf_contract //release:plan_contract --test_output=errors --jobs=8
./bazel test --config=profile //release:elf_contract --test_output=errors --jobs=8
./bazel test --config=release //release:contract //release:elf_contract //release:plan_contract --test_output=errors --jobs=8
./bazel build --config=release //release:load --output_groups=+tarball --jobs=8
```

The tarball output group is the pinned upstream `oci_load` API. An earlier
attempt at nonexistent `//release:load.tarball` failed during target selection,
not image compilation. Receipt `018359f5-1cca-468c-87c2-72e7b2f916a9` loaded the
produced tarball into rootless Podman, asserted amd64, user `65532:65532`,
entrypoint `/app/pulsebeam`, workdir `/app`, and ports `3478/udp` and `7070/tcp`.
The non-root image passed `--help` with `--network=none`. Nothing was published.

Receipt `8aa01d1a-d4d1-400f-825c-3c896bb93307` repeated the shared-cache proof
with all three worktrees synchronized to the repaired candidate and their
output state expunged. A executed the routing Rustc/test actions; concurrent B/C
each restored all 111 logged spawns, including one routing test-binary Rustc
and two TestRunner actions, from the persistent shared disk cache. Output bases
and daemon PIDs were distinct, and the forged runtime identity was rejected.
Original logs and the summary are retained locally under
`/home/lukas/.cache/pulsebeam/cache-proof.9pltBF/evidence-browser-runtime/`.

Complete acceptance coverage therefore combines the unaffected passing aggregate
results with all affected browser/native reruns, rather than repeating slow
simulation acceptance or claiming the earlier aggregate returned success.
The original empty-cache, native-aarch64 and native-DTLS qualifications above
remain unchanged. This does not establish Fedora installation, GitHub CI
execution or production publication. Earlier real Neovim evidence is retained;
the renewed Rust launcher/PATH effect was source-reviewed without finding a
concrete routing break, not exercised in a renewed Neovim session.

## Final handoff approval

Final launcher/workflow/plan and whitespace checks passed receipt
`b5680081-4715-4917-8807-81341687ebbd` after recording the repaired-candidate
evidence. The human then explicitly approved all three consequential outcomes:

- Renewed Bazel/tooling and test ownership with the stated validation
  qualifications and retained editor evidence.
- Native distribution and the amd64 image, retaining the previously accepted
  native-aarch64 execution waiver.
- Persistent shared local disk caching with isolated output state and the
  refreshed concurrent Rustc/TestRunner reuse proof.

These approvals accepted the qualified review handoff. The human subsequently
explicitly authorized a local commit of the approved migration changes,
superseding the original review-only no-commit endpoint. This does not authorize
merge, push or production publication. No additional exclusions or validation
claims were introduced.

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
