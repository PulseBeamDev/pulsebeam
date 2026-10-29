# pulsebeam-udp

Bounded UDP socket I/O for single-owner event loops. Bind a socket, attach any
host-owned Linux reuseport program before activation, then drive readiness and
nonblocking send/receive calls. The crate does not own routing, advertised
addresses, protocol state, pacing, retries, or network simulation policy.

```rust
use pulsebeam_udp::{BindOptions, SendDatagram, SendStop};

# async fn example() -> std::io::Result<()> {
let bound = pulsebeam_udp::bind(BindOptions::new("127.0.0.1:0".parse().unwrap())).await?;
let loopback = bound.local_addr();
let mut socket = bound.activate()?;
socket.writable().await?;
let entries = [SendDatagram {
    destination: loopback,
    payload: b"complete datagram",
    gso_segment_size: None,
}];
let report = socket.try_send_batch(&entries);
let unsent = &entries[report.accepted..];
match report.stop {
    None => assert!(unsent.is_empty()),
    Some(SendStop::Blocked) => return Err(std::io::ErrorKind::WouldBlock.into()),
    Some(SendStop::Error(error)) => return Err(error),
}
socket.readable().await?;
match socket.try_recv_batch() {
    Ok(batch) => {
        for datagram in batch.iter() {
            // Inspect borrowed bytes here; copy explicitly for an owned handoff.
            assert!(!datagram.destination.ip().is_unspecified());
        }
    }
    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
    Err(error) => return Err(error),
}
# Ok(())
# }
```

`accepted` counts a contiguous prefix of input entries submitted to the local
backend, not delivered packets. A GSO aggregate is one atomic entry. All entries
are validated before any send. Calls accept at most 32 input entries and reject
larger batches before submission; short success,
`WouldBlock`, and errors retain exact progress and leave the suffix unsubmitted.
There is no application send queue. Socket-local counters distinguish submission,
receive, local invalid/truncated drops, blocked calls, and errors, not network loss.

Receive slices borrow reusable socket storage until the batch is dropped. GRO is
split into individual logical datagrams, including a final short segment. Source,
actual local destination, and original receive time remain attached to each item.
The default 1500-byte receive ceiling is a memory/safety bound, not path MTU; set
`max_datagram_bytes` up to 65507 for larger payloads. Kernel messages and GRO
segments are independently bounded at 32 and 64.

Linux uses safe nix batch syscalls, including destination pktinfo in scalar mode.
Offloads are probed; callers must inspect capabilities before building GSO entries.
Explicit `Batch` fails on platforms without batching; `Auto` selects scalar there.
The portable scalar backend requires a specific local address and separate IPv4
and IPv6 sockets because it cannot recover wildcard/dual-stack destination metadata.
Only the pre-activation Linux socket exposes its FD.

With `sim`, the same concrete API uses virtual time. A simple specifically bound
Turmoil socket works directly; a host needing reuseport, steering, GSO/GRO, or
network effects supplies `simulation::Io`. This bounded device adapter must accept
an entire entry atomically, before network loss/delay/reordering. The SFU's adapter
and classifier live in `pulsebeam-runtime`, not in this crate.

## Contributor proof

The conformance target requires Linux dual-stack loopback with UDP GSO/GRO enabled;
unsupported host capabilities fail explicitly. See the
[testing contract](../../docs/build.md#testing-contract) for owner and final gates.

```sh
./bazel test //crates/pulsebeam-udp:unit_tests //crates/pulsebeam-udp:simulation_tests
./bazel test //crates/pulsebeam-udp:format //crates/pulsebeam-udp:clippy
./bazel test //crates/pulsebeam-udp:transport_conformance
```

## Measure I/O cost

```sh
./bazel run --config=profile //crates/pulsebeam-udp:benchmark -- 100000
```

This Linux-only, single-owner loopback benchmark prints ordinary batches of
1/8/32 entries, GSO groups of 4/16/64 segments, and a mixed grouping. Payloads
are a fixed synthetic distribution of 80/900/1200/2048-byte packets, not a claim
about production traffic frequencies. Each GSO group has 800-byte segments and
a shorter tail. Scalar mode measures ordinary traffic only.

Output includes logical packets and bytes per second, thread CPU seconds and
CPU nanoseconds per packet, allocation calls per logical packet, and p99/p99.99
send and receive processing nanoseconds per exchange. Processing includes
syscalls and receive iteration but excludes readiness waits. CPU uses Linux
thread accounting and `getconf CLK_TCK`; allocation instrumentation delegates to
the system allocator and adds counter overhead. The harness preallocates timing
samples and warms sockets before measurement. Results are measurements, not
correctness thresholds or network end-to-end latency claims.

Compare identical sample counts, workload/harness source, build profile, CPU
placement and host conditions in separate baseline/candidate worktrees. Adapt
only the socket API boundary when comparing an older implementation. Keep raw
results outside the repository; throughput alone is insufficient without CPU,
allocation and tail measurements.
