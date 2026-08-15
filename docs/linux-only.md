# PulseBeam server: Linux-only

The server's preferred UDP steering path is Aya/eBPF attached to a
`SO_REUSEPORT` group (`BPF_PROG_TYPE_SK_REUSEPORT` selecting a socket via
`SO_ATTACH_REUSEPORT_EBPF`). Userspace forwarding remains the correctness
fallback for bootstrap and for hosts where the object is absent.

## Compile-time gate

The gate is already present in `pulsebeam/src/lib.rs`, before the server
modules. Portable protocol, core, and simulator crates remain buildable on
other targets; the server crate fails immediately there.

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
- A missing eBPF object uses the userspace forwarding path. If an object is
  present but a capability or verifier error prevents loading or attaching it,
  startup fails explicitly rather than claiming that steering is active. The
  attached state is exported as a metric.

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

## Building the eBPF program

The eBPF package owns its Cargo configuration. From the repository root, both
of these commands build the same `bpfel-unknown-none` release artifact:

```sh
make build-ebpf
(cd pulsebeam-ebpf && cargo build --release)
```

The package-local `rust-toolchain.toml` selects nightly with `rust-src`, and
`.cargo/config.toml` selects the target and `build-std = ["core"]`. The package
build script resolves `bpf-linker` automatically only for that BPF target:

1. `BPF_LINKER`, when set;
2. an executable `bpf-linker` already on `PATH`;
3. Aya's pinned prebuilt `bpf-linker` 0.11.0 release for the host platform.

Downloaded archives are SHA-256 checked and cached below Cargo's target
directory, so subsequent builds need no network access. The automatic release
path currently supports Linux x86_64/aarch64 and macOS x86_64/aarch64. Other
hosts can use `BPF_LINKER=/path/to/bpf-linker`. A first download needs `curl`
and a `tar` with zstd support.

Host-target checks such as `cargo check -p pulsebeam-ebpf` do not resolve or
download the linker; they compile the shared classifier without running the BPF
verifier.
