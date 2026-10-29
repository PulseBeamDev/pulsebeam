# `pulsebeam-queue`

Bounded ownership handoff for many producers and one logical receiver. Use
`try_send` on realtime paths; the caller decides what to do with a rejected
value. Acceptance means queued, not delivered or processed.

```rust
use core::num::NonZeroUsize;
use pulsebeam_queue::{bounded, TrySendError};

let (tx, mut rx) = bounded(NonZeroUsize::MIN);
tx.try_send(42).unwrap();
assert_eq!(tx.try_send(43), Err(TrySendError::Full(43)));
assert_eq!(rx.try_recv(), Ok(42));
```

Capacity counts every accepted value until received. `Full(T)` and `Closed(T)`
return ownership. Dropping all producers leaves accepted values drainable.
Receive methods require exclusive access; the receiver can move between workers
but cannot be cloned or concurrently driven. Sequential successful sends from
each producer retain their order.

`readable().await` waits for data without consuming it or releasing capacity.
It is safe to cancel and restart in select loops, returns true immediately while
data remains, and returns false only after closure and draining.
`try_recv_batch(out, max)` appends at most the caller's budget, does nothing for
zero, and allocates only if the caller's vector needs to grow.

`send().await` is a control-plane convenience that waits for capacity. Cancelling
a pending send drops its unaccepted value; closure returns it as `SendError(T)`.
It is not a realtime send policy. The crate owns no scheduling, timers, metrics,
transport semantics or saturation recovery.

## Backend and development

Build and test through the repository's [Bazel workflow](../../docs/build.md):

```sh
./bazel test //crates/pulsebeam-queue:unit_tests //crates/pulsebeam-queue:format //crates/pulsebeam-queue:clippy
```

The backend is pinned tachyonix 0.3.1. The Bazel-owned
[readiness patch](../../tools/patches/tachyonix-readable.patch) exposes its existing
slot predicate and receiver waker without modifying push/pop. It avoids a
prefetched item outside the bounded ring. Cargo manifests supply dependency
metadata; Bazel supplies the patched dependency, as with other repository
dependency patches. Warm immediate sends and receives move values without
allocation or a global lock. Async waiting uses the backend's wakeup machinery.

Run the advisory paired benchmark separately from other CPU-heavy work:

```sh
./bazel run --config=profile //crates/pulsebeam-queue:benchmark
```

It compares the pre-extraction tachyonix mailbox hot-path shape with this queue,
alternating execution order over three rounds. It reports p50/p99.99 immediate
send/receive and cross-worker handoff times, process CPU time, construction
allocations/bytes, and warm-path allocations. The four-producer case mixes
32-byte inline and 1,200-byte owned payloads, bursts, full rejections and bounded
consumer pauses. Timing reports are not pass/fail thresholds; hardware cache
counters require separate host profiling.

SFU lane capacities and per-tick budgets remain in the SFU, not this crate.
There is no dependency on PulseBeam runtime, transports or media types.
