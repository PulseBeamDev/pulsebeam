# PulseBeam server: Linux-only

The server's UDP steering path is Aya/eBPF attached to a `SO_REUSEPORT`
group (`BPF_PROG_TYPE_SK_REUSEPORT` selecting a socket via
`SO_ATTACH_REUSEPORT_EBPF`). There is no non-Linux fallback, and none should
be added — a "portable" code path here would silently stop doing the thing
this crate exists to do.

## Apply this compile-time gate

This snippet needs to land in `pulsebeam/src/lib.rs`, near the top of the
file (before any other item), by whoever owns that file — it is intentionally
not applied here because `pulsebeam/src/**` is out of scope for this change.

```rust
#[cfg(not(target_os = "linux"))]
compile_error!(
    "pulsebeam server requires Linux: its UDP steering path is Aya/eBPF \
     (BPF_PROG_TYPE_SK_REUSEPORT). Portable crates (protocol, core, \
     simulator) build elsewhere; the server binary does not."
);
```

Place it before `mod` declarations so a non-Linux build fails immediately
rather than partway through name resolution.

## Minimum supported Linux kernel

- **Hard floor: Linux 4.19.** `BPF_PROG_TYPE_SK_REUSEPORT` — the program
  type that lets an eBPF program pick which member of a `SO_REUSEPORT` group
  receives a packet — was added in 4.19
  ([torvalds/linux@2dbb9b9](https://github.com/torvalds/linux/commit/2dbb9b9e6df67d444fbe425c7f6014858d337ad)).
  Kernels older than this cannot run the loader at all: the `bpf(BPF_PROG_LOAD)`
  call fails with an unknown program type.
- **Recommended floor: Linux 5.8.** This is where `CAP_BPF` was split out of
  `CAP_SYS_ADMIN` ([LWN #820560](https://lwn.net/Articles/820560/)). Below
  5.8 the loader needs the much broader `CAP_SYS_ADMIN` just to load an
  `SK_REUSEPORT` program; at 5.8+ the narrower `CAP_BPF` is enough (see
  below). Treat 5.8 as the practical minimum for anything that isn't running
  fully privileged, and prefer a current LTS (5.15, 6.1, 6.6, ...) in
  production.

## Required capabilities

- **Loading the `SK_REUSEPORT` program (`bpf(BPF_PROG_LOAD)`):**
  - Kernel >= 5.8: `CAP_BPF` is sufficient. `SK_REUSEPORT` was deliberately
    special-cased to not additionally require `CAP_NET_ADMIN` — it was
    reasoned to be equivalent to the already-unprivileged `SOCKET_FILTER`
    type. Map creation and other `sys_bpf()` commands the loader needs also
    fall under `CAP_BPF`.
  - Kernel < 5.8: `CAP_SYS_ADMIN` (there is no `CAP_BPF` to hold instead).
- **Attaching the program to the listening socket**
  (`setsockopt(SO_ATTACH_REUSEPORT_EBPF)`): no additional capability beyond
  ordinary socket ownership of the `fd` being configured.
- In practice, grant the server binary (or its systemd unit) `CAP_BPF` plus
  `CAP_NET_ADMIN` together rather than relying on the `SK_REUSEPORT`
  special case precisely — `CAP_NET_ADMIN` is what most other networking
  program types need, ops tooling and systemd's `AmbientCapabilities=`
  presentation generally assume the pair, and the special case is a detail
  of this one program type that is easy to invalidate with an unrelated
  future BPF program. Do not grant `CAP_SYS_ADMIN` on a 5.8+ kernel — it is
  strictly broader than what the loader needs.
- A missing or insufficient capability must fail the Linux server startup
  path explicitly (a rejected `bpf()` syscall surfaced as a startup error),
  not silently fall back to a non-eBPF steering path.

## CI mapping

- The unprivileged build/verifier gate (`make build-ebpf`, `cargo test -p
  pulsebeam-routing`) runs in the normal `build` job in `.github/workflows/ci.yml`
  and needs no elevated capability — it only compiles the eBPF object and
  exercises the shared classifier in userspace.
- The privileged attach/load smoke test — which actually loads the compiled
  object into a running kernel and exercises `SO_ATTACH_REUSEPORT_EBPF` — runs
  in the separate `ebpf-smoke` job under `sudo`, kept distinct from `build` so
  a kernel/capability failure there can never mask or be masked by an
  ordinary compile/test failure.

## Building `bpf-linker`

`cargo install bpf-linker --locked` against a stock LLVM does **not** work, and
the failure is not a missing header or a permissions problem. bpf-linker 0.11.0
links `LLVMParseIRInContext2`, which stock LLVM does not export — Aya carries a
patched LLVM and ships prebuilt binaries for this reason, and the crate's own
build script warns that `cargo install` is not the supported path.

Verified here against LLVM 21.1.8: the build gets all the way to linking and
fails with `rust-lld: error: undefined symbol: LLVMParseIRInContext2`. Setting
`LLVM_SYS_211_PREFIX` does not help; it selects which LLVM is used, not which
symbols it has.

So CI must install bpf-linker from Aya's prebuilt release rather than building
it, and `make build-ebpf` cannot run on a machine that only has a distribution
LLVM. `cargo check -p pulsebeam-ebpf` (host target) still compiles the program
and its use of the shared classifier, which is the part worth gating on every
change; it just does not run the BPF verifier.
