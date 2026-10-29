//! Advisory paired measurements against the pre-extraction mailbox hot path.
//! Timings are observations, never flaky acceptance thresholds.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "isolated benchmark measures allocations, host CPU time and real cross-worker handoff"
)]
#![expect(
    clippy::print_stdout,
    reason = "this standalone benchmark reports measurements to its invoking developer"
)]

use pulsebeam_queue::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::num::NonZeroUsize;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

struct Allocator;
thread_local! {
    static ALLOCATIONS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}
fn record(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some((allocations, total)) = count.get() {
            count.set(Some((allocations + 1, total + bytes)));
        }
    });
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn allocations<T>(f: impl FnOnce() -> T) -> (T, (usize, usize)) {
    ALLOCATIONS.with(|count| count.set(Some((0, 0))));
    let result = f();
    let counts = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    (result, counts)
}

trait Lane {
    type Tx: Clone + Send;
    type Rx: Send;
    fn make(capacity: usize) -> (Self::Tx, Self::Rx);
    fn send(tx: &Self::Tx, value: Message) -> Result<(), TrySendError<Message>>;
    fn recv(rx: &mut Self::Rx) -> Result<Message, TryRecvError>;
}
struct Queue;
impl Lane for Queue {
    type Tx = Sender<Message>;
    type Rx = Receiver<Message>;
    fn make(capacity: usize) -> (Self::Tx, Self::Rx) {
        bounded(NonZeroUsize::new(capacity).unwrap())
    }
    fn send(tx: &Self::Tx, value: Message) -> Result<(), TrySendError<Message>> {
        tx.try_send(value)
    }
    fn recv(rx: &mut Self::Rx) -> Result<Message, TryRecvError> {
        rx.try_recv()
    }
}

// Frozen shape of pulsebeam-runtime::mailbox before extraction, including its
// pending-first branch. It shares tachyonix's unchanged push/pop implementation.
struct Mailbox;
struct MailboxReceiver {
    inner: tachyonix::Receiver<Message>,
    pending: Option<Message>,
}
impl Lane for Mailbox {
    type Tx = tachyonix::Sender<Message>;
    type Rx = MailboxReceiver;
    fn make(capacity: usize) -> (Self::Tx, Self::Rx) {
        let (tx, inner) = tachyonix::channel(capacity);
        (
            tx,
            MailboxReceiver {
                inner,
                pending: None,
            },
        )
    }
    fn send(tx: &Self::Tx, value: Message) -> Result<(), TrySendError<Message>> {
        tx.try_send(value)
    }
    fn recv(rx: &mut Self::Rx) -> Result<Message, TryRecvError> {
        if let Some(value) = rx.pending.take() {
            return Ok(value);
        }
        rx.inner.try_recv()
    }
}

#[derive(Debug)]
enum Payload {
    Small([u8; 32]),
    Large(Box<[u8; 1200]>),
}
#[derive(Debug)]
struct Message {
    sent: Instant,
    payload: Payload,
}
impl Message {
    fn new(sequence: usize) -> Self {
        Self {
            sent: Instant::now(),
            payload: if sequence.is_multiple_of(8) {
                Payload::Large(Box::new([7; 1200]))
            } else {
                Payload::Small([3; 32])
            },
        }
    }
    fn observe(&self) {
        match &self.payload {
            Payload::Small(bytes) => {
                black_box(bytes);
            }
            Payload::Large(bytes) => {
                black_box(bytes);
            }
        }
    }
}
fn cpu_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: the host call writes one initialized timespec owned by this stack.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
        0
    );
    Duration::new(
        time.tv_sec.try_into().unwrap(),
        time.tv_nsec.try_into().unwrap(),
    )
}
fn report(label: &str, mut samples: Vec<u64>) {
    samples.sort_unstable();
    if !samples.is_empty() {
        println!(
            "{label}: n={} p50={}ns p99.99={}ns",
            samples.len(),
            samples[samples.len() / 2],
            samples[(samples.len() - 1) * 9999 / 10000]
        );
    }
}
fn sequential<L: Lane>(label: &str) {
    let ((tx, mut rx), memory) = allocations(|| L::make(1024));
    let mut message = Message::new(0);
    L::send(&tx, message).unwrap();
    message = L::recv(&mut rx).unwrap();
    let mut send_times = Vec::with_capacity(200_000);
    let mut recv_times = Vec::with_capacity(200_000);
    let cpu = cpu_time();
    let wall = Instant::now();
    let (_, warm) = allocations(|| {
        for _ in 0..200_000 {
            let before = Instant::now();
            L::send(&tx, black_box(message)).unwrap();
            send_times.push(before.elapsed().as_nanos() as u64);
            let before = Instant::now();
            message = L::recv(&mut rx).unwrap();
            recv_times.push(before.elapsed().as_nanos() as u64);
            message.observe();
        }
    });
    println!(
        "{label}/roundtrip: wall={:?} cpu={:?} construction_allocations/bytes={memory:?} warm_allocations/bytes={warm:?}",
        wall.elapsed(),
        cpu_time() - cpu
    );
    assert_eq!(warm, (0, 0), "warm immediate moves must not allocate");
    report(&format!("{label}/send"), send_times);
    report(&format!("{label}/recv"), recv_times);
}
fn cross_worker<L: Lane>(label: &str) {
    const PRODUCERS: usize = 4;
    const ATTEMPTS: usize = 100_000;
    let (tx, mut rx) = L::make(1024);
    let barrier = Arc::new(Barrier::new(PRODUCERS + 1));
    // Allocate payloads and output buffers outside the timed interval.
    let pools: Vec<_> = (0..PRODUCERS)
        .map(|_| (0..ATTEMPTS).map(Message::new).collect::<Vec<_>>())
        .collect();
    let mut received = Vec::with_capacity(PRODUCERS * ATTEMPTS);
    let mut recv_times = Vec::with_capacity(PRODUCERS * ATTEMPTS);
    let mut latency = Vec::with_capacity(PRODUCERS * ATTEMPTS);
    let cpu = cpu_time();
    let wall = Instant::now();
    std::thread::scope(|scope| {
        let workers: Vec<_> = pools
            .into_iter()
            .map(|pool| {
                let tx = tx.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let mut times = Vec::with_capacity(ATTEMPTS);
                    let mut rejected = Vec::with_capacity(ATTEMPTS);
                    barrier.wait();
                    for (sequence, mut message) in pool.into_iter().enumerate() {
                        message.sent = Instant::now();
                        let start = Instant::now();
                        let result = L::send(&tx, message);
                        times.push(start.elapsed().as_nanos() as u64);
                        if let Err(TrySendError::Full(value)) = result {
                            rejected.push(value);
                        } else {
                            assert!(result.is_ok());
                        }
                        if sequence.is_multiple_of(64) {
                            std::thread::yield_now();
                        }
                    }
                    (times, rejected)
                })
            })
            .collect();
        drop(tx);
        barrier.wait();
        loop {
            let start = Instant::now();
            match L::recv(&mut rx) {
                Ok(value) => {
                    recv_times.push(start.elapsed().as_nanos() as u64);
                    latency.push(value.sent.elapsed().as_nanos() as u64);
                    value.observe();
                    received.push(value);
                    if received.len().is_multiple_of(256) {
                        // Deliberate bounded consumer pauses, not sleeps.
                        for _ in 0..32 {
                            std::hint::spin_loop();
                        }
                    }
                }
                Err(TryRecvError::Empty) => std::hint::spin_loop(),
                Err(TryRecvError::Closed) => break,
            }
        }
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        println!(
            "{label}/4-worker-bursty-stalled: wall={:?} cpu={:?} accepted={} full={}",
            wall.elapsed(),
            cpu_time() - cpu,
            received.len(),
            results.iter().map(|(_, r)| r.len()).sum::<usize>()
        );
        report(
            &format!("{label}/contended-send-including-full"),
            results.into_iter().flat_map(|(times, _)| times).collect(),
        );
    });
    report(&format!("{label}/contended-recv"), recv_times);
    report(&format!("{label}/cross-worker-handoff"), latency);
}
fn main() {
    for round in 0..3 {
        println!("paired round {round}, mixed 32/1200-byte owned payloads, capacity 1024");
        // Alternate order to expose host drift rather than hiding it.
        if round % 2 == 0 {
            sequential::<Mailbox>("mailbox");
            sequential::<Queue>("queue");
            cross_worker::<Mailbox>("mailbox");
            cross_worker::<Queue>("queue");
        } else {
            sequential::<Queue>("queue");
            sequential::<Mailbox>("mailbox");
            cross_worker::<Queue>("queue");
            cross_worker::<Mailbox>("mailbox");
        }
    }
}
