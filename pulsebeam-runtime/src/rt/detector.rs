/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// NOTE: This file was copied from https://github.com/facebookexperimental/rust-shed/blob/main/shed/tokio-detectors/src/detectors.rs
// to avoid a dependency link and for easier local maintenance.

//! # Tokio runtime health monitoring detector(s).
//!
//! ## Detecting blocked tokio workers: [`LongRunningTaskDetector`].
//!
//! Blocking IO operations often end up in tokio workers negatively impacting the ability of tokio runtimes to process
//! async requests. The simplest example of this is the use of [`std::thread::sleep`] instead of [`tokio::time::sleep`] which we use
//! in the unit tests to test this utility.
//!
//! The aim of this utility is to detect these situations.
//! [`LongRunningTaskDetector`] is designed to be very low overhead so that it can be safely be run in production.
//! The overhead of this utility is vconfigurable via probing `interval` parameter.
//!
//! ### [`LongRunningTaskDetector`] Example:
//!
//! ```
//! use std::sync::Arc;
//!
//! use tokio_detectors::detectors::LongRunningTaskDetector;
//!
//! let (lrtd, mut builder) = LongRunningTaskDetector::new_multi_threaded(
//!     std::time::Duration::from_millis(10),
//!     std::time::Duration::from_millis(100),
//! );
//! let runtime = builder.worker_threads(2).enable_all().build().unwrap();
//! let runtime_ref = Arc::new(runtime);
//! let lrtd_runtime_ref = runtime_ref.clone();
//! lrtd.start(lrtd_runtime_ref);
//! runtime_ref.block_on(async { print!("my async code") });
//! ```
//!
//! The above will allow you to get details on what is blocking your tokio worker threads for longer that 100ms.
//! The detail with default action handler will look like:
//!
//! ```text
//! Detected blocking in worker threads: [
//!  ThreadInfo { id: ThreadId(10), pthread_id: 123145381474304 },
//!  ThreadInfo { id: ThreadId(11), pthread_id: 123145385693184 }
//! ]
//! ```
//!
//! To get more details(like stack traces) start [`LongRunningTaskDetector`] with [`LongRunningTaskDetector::start_with_custom_action`]
//! and provide a custom handler([`BlockingActionHandler`]) that can dump the thread stack traces. The [`LongRunningTaskDetector`] integration tests
//! include an example implementation that is not signal safe as an example.
//! More detailed blocking can look like:
//!
//! ```text
//! Blocking detected with details: 123145387802624
//! Stack trace for thread tokio-runtime-worker(123145387802624):
//! ...
//!   5: __sigtramp
//!   6: ___semwait_signal
//!   7: <unknown>
//!   8: std::sys::pal::unix::thread::Thread::sleep
//!             at /rustc/.../library/std/src/sys/pal/unix/thread.rs:243:20
//!   9: std::thread::sleep
//!             at /rustc/.../library/std/src/thread/mod.rs:869:5
//!  10: detectors::unix_lrtd_tests::run_blocking_stuff::{{closure}}
//!             at ./tests/detectors.rs:98:9
//!  11: tokio::runtime::task::core::Core<T,S>::poll::{{closure}}
//! ...
//!
//! which will help you easily identify the blocking operation(s).
//! ```

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::thread::ThreadId;
use std::time::Duration;

use rand::Rng;
use rand::rng;
use tokio::runtime::Builder;
use tokio::runtime::Runtime;

/// the maximum duration for tokio workers to be blocked after which the process is considered a lost cause, and we will panic.
const PANIC_WORKER_BLOCK_DURATION_DEFAULT: Duration = Duration::from_mins(1);

fn get_panic_worker_block_duration() -> Duration {
    let duration_str =
        env::var("PANIC_WORKER_BLOCK_DURATION_DEFAULT").unwrap_or_else(|_| "60".to_string());
    duration_str
        .parse::<u64>()
        .map(Duration::from_secs)
        .unwrap_or(PANIC_WORKER_BLOCK_DURATION_DEFAULT)
}

#[cfg(unix)]
fn get_thread_id() -> libc::pthread_t {
    unsafe { libc::pthread_self() }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ThreadInfo {
    id: ThreadId,
    #[cfg(unix)]
    pthread_id: libc::pthread_t,
}

/// A structure to hold information about a thread, including its platform-specific identifiers.
impl ThreadInfo {
    fn new() -> Self {
        ThreadInfo {
            id: thread::current().id(),
            #[cfg(unix)]
            pthread_id: get_thread_id(),
        }
    }

    /// Returns the id
    #[allow(dead_code)]
    pub fn id(&self) -> &ThreadId {
        &self.id
    }

    /// Returns the `pthread_id` of this thread.
    #[cfg(unix)]
    #[cfg_attr(docsrs, doc(cfg(unix)))]
    pub fn pthread_id(&self) -> &libc::pthread_t {
        &self.pthread_id
    }
}

/// A trait for handling actions when blocking is detected.
///
/// This trait provides a method for handling the detection of a blocking action.
pub trait BlockingActionHandler: Send + Sync {
    /// Called when a blocking action is detected and prior to thread signaling.
    ///
    /// # Arguments
    ///
    /// * `workers` - The list of thread IDs of the tokio runtime worker threads.
    fn blocking_detected(&self, workers: &[ThreadInfo]);

    /// Called when the runtime recovers from a detected blocking event (i.e. the
    /// probe task completes successfully after a prior detection).  Implementors
    /// can use this to reset accumulated state such as first-detection timestamps.
    ///
    /// The default implementation is a no-op.
    fn blocking_resolved(&self) {}
}

impl<F> BlockingActionHandler for F
where
    F: Fn(&[ThreadInfo]) + Send + Sync,
{
    fn blocking_detected(&self, workers: &[ThreadInfo]) {
        self(workers);
    }
    // blocking_resolved: default no-op is sufficient for closure impls.
}
#[cfg_attr(unix, allow(dead_code))]
struct StdErrBlockingActionHandler;

/// BlockingActionHandler implementation that writes blocker details to standard error.
impl BlockingActionHandler for StdErrBlockingActionHandler {
    fn blocking_detected(&self, workers: &[ThreadInfo]) {
        eprintln!("Detected blocking in worker threads: {:?}", workers);
    }
}

#[derive(Debug)]
struct WorkerSet {
    inner: Mutex<HashSet<ThreadInfo>>,
}

impl WorkerSet {
    fn new() -> Self {
        WorkerSet {
            inner: Mutex::new(HashSet::new()),
        }
    }

    fn add(&self, pid: ThreadInfo) {
        let mut set = self.inner.lock().unwrap();
        set.insert(pid);
    }

    fn remove(&self, pid: ThreadInfo) {
        let mut set = self.inner.lock().unwrap();
        set.remove(&pid);
    }

    fn get_all(&self) -> Vec<ThreadInfo> {
        let set = self.inner.lock().unwrap();
        set.iter().cloned().collect()
    }
}

/// Worker health monitoring detector to help with detecting blocking in tokio workers.
///
/// Blocking IO operations often end up in tokio workers negatively impacting the ability of tokio runtimes to process
/// async requests. The simplest example of this is the use of [`std::thread::sleep`] instead of [`tokio::time::sleep`] which we use
/// in the unit tests to test this utility.
///
/// The aim of this utility is to detect these situations.
/// [`LongRunningTaskDetector`] is designed to be very low overhead so that it can be safely be run in production.
/// The overhead of this utility is vconfigurable via probing `interval` parameter.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
///
/// use tokio_detectors::detectors::LongRunningTaskDetector;
///
/// let (lrtd, mut builder) = LongRunningTaskDetector::new_multi_threaded(
///     std::time::Duration::from_millis(10),
///     std::time::Duration::from_millis(100),
/// );
/// let runtime = builder.worker_threads(2).enable_all().build().unwrap();
/// let runtime_ref = Arc::new(runtime);
/// let lrtd_runtime_ref = runtime_ref.clone();
/// lrtd.start(lrtd_runtime_ref);
/// runtime_ref.block_on(async { print!("my async code") });
/// ```
///
/// The above will allow you to get details on what is blocking your tokio worker threads for longer that 100ms.
/// The detail with default action handler will look like:
///
/// ```text
/// Detected blocking in worker threads: [
///  ThreadInfo { id: ThreadId(10), pthread_id: 123145381474304 },
///  ThreadInfo { id: ThreadId(11), pthread_id: 123145385693184 }
/// ]
/// ```
///
/// To get more details(like stack traces) start [`LongRunningTaskDetector`] with [`LongRunningTaskDetector::start_with_custom_action`]
/// and provide a custom handler([`BlockingActionHandler`]) that can dump the thread stack traces. The [`LongRunningTaskDetector`] integration tests
/// include an example implementation that is not signal safe as an example.
/// More detailed blocking can look like:
///
/// ```text
/// Blocking detected with details: 123145387802624
/// Stack trace for thread tokio-runtime-worker(123145387802624):
/// ...
///   5: __sigtramp
///   6: ___semwait_signal
///   7: <unknown>
///   8: std::sys::pal::unix::thread::Thread::sleep
///             at /rustc/.../library/std/src/sys/pal/unix/thread.rs:243:20
///   9: std::thread::sleep
///             at /rustc/.../library/std/src/thread/mod.rs:869:5
///  10: detectors::unix_lrtd_tests::run_blocking_stuff::{{closure}}
///             at ./tests/detectors.rs:98:9
///  11: tokio::runtime::task::core::Core<T,S>::poll::{{closure}}
/// ...
///
/// which will help you easily identify the blocking operation(s).
/// ```
#[derive(Debug)]
pub struct LongRunningTaskDetector {
    interval: Duration,
    detection_time: Duration,
    stop_flag: Arc<Mutex<bool>>,
    workers: Arc<WorkerSet>,
}

async fn do_nothing(tx: mpsc::Sender<()>) {
    // signal I am done
    tx.send(()).unwrap();
}

fn probe(
    tokio_runtime: &Arc<Runtime>,
    detection_time: Duration,
    workers: &Arc<WorkerSet>,
    action: &Arc<dyn BlockingActionHandler>,
) {
    let (tx, rx) = mpsc::channel();
    let _nothing_handle = tokio_runtime.spawn(do_nothing(tx));
    let is_probe_success = match rx.recv_timeout(detection_time) {
        Ok(_result) => true,
        Err(_) => false,
    };
    if !is_probe_success {
        let targets = workers.get_all();
        action.blocking_detected(&targets);
        // Wait for the runtime to recover; panic if it never does.
        match rx.recv_timeout(get_panic_worker_block_duration()) {
            Ok(_) => {
                // Runtime recovered – reset accumulated detection state.
                action.blocking_resolved();
            }
            Err(_) => {
                panic!(
                    "Tokio worker threads have been blocked for more than {:?}. \
                     See the blocking report printed above for the culprit location.",
                    get_panic_worker_block_duration()
                );
            }
        }
    }
}

impl LongRunningTaskDetector {
    /// Creates [`LongRunningTaskDetector`] and a current threaded [`tokio::runtime::Builder`].
    ///
    /// The `interval` argument determines the time interval between tokio runtime worker probing.
    /// This interval is randomized.
    ///
    /// The `detection_time` argument determines maximum time allowed for a probe to succeed.
    /// A probe running for longer is considered a tokio worker health issue. (something is blocking the worker threads)
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use tokio_detectors::detectors::LongRunningTaskDetector;
    ///
    /// let (detector, builder) = LongRunningTaskDetector::new_current_threaded(
    ///     Duration::from_secs(1),
    ///     Duration::from_secs(5),
    /// );
    /// ```
    pub fn new_current_threaded(interval: Duration, detection_time: Duration) -> (Self, Builder) {
        let workers = Arc::new(WorkerSet::new());
        workers.add(ThreadInfo::new());
        let runtime_builder = Builder::new_current_thread();
        (
            LongRunningTaskDetector {
                interval,
                detection_time,
                stop_flag: Arc::new(Mutex::new(true)),
                workers,
            },
            runtime_builder,
        )
    }

    /// Creates [`LongRunningTaskDetector`] and a multi threaded [`tokio::runtime::Builder`].
    ///
    /// The `interval` argument determines the time interval between tokio runtime worker probing.
    /// This `interval`` is randomized.
    ///
    /// The `detection_time` argument determines maximum time allowed for a probe to succeed.
    /// A probe running for longer is considered a tokio worker health issue. (something is blocking the worker threads)
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use tokio_detectors::detectors::LongRunningTaskDetector;
    ///
    /// let (detector, builder) =
    ///     LongRunningTaskDetector::new_multi_threaded(Duration::from_secs(1), Duration::from_secs(5));
    /// ```
    pub fn new_multi_threaded(interval: Duration, detection_time: Duration) -> (Self, Builder) {
        let workers = Arc::new(WorkerSet::new());
        let mut runtime_builder = Builder::new_multi_thread();
        let workers_clone = Arc::clone(&workers);
        let workers_clone2 = Arc::clone(&workers);
        runtime_builder
            .on_thread_start(move || {
                // Only register actual async worker threads; tokio's blocking pool
                // threads are named "tokio-blocking-N" and must not be monitored
                // (they legitimately block, and would produce false positives).
                let is_worker = std::thread::current()
                    .name()
                    .map(|n| n.starts_with("tokio-runtime-worker"))
                    .unwrap_or(false);
                if is_worker {
                    workers_clone.add(ThreadInfo::new());
                }
            })
            .on_thread_stop(move || {
                let is_worker = std::thread::current()
                    .name()
                    .map(|n| n.starts_with("tokio-runtime-worker"))
                    .unwrap_or(false);
                if is_worker {
                    workers_clone2.remove(ThreadInfo::new());
                }
            });
        (
            LongRunningTaskDetector {
                interval,
                detection_time,
                stop_flag: Arc::new(Mutex::new(true)),
                workers,
            },
            runtime_builder,
        )
    }

    /// Starts the monitoring thread with default action handlers (that write details to std err).
    pub fn start(&self, runtime: Arc<Runtime>) {
        let handler = {
            #[cfg(unix)]
            {
                unix::install_default_stack_trace_handler();
                Arc::new(unix::DetailedCaptureBlockingActionHandler::new())
            }
            #[cfg(not(unix))]
            {
                Arc::new(StdErrBlockingActionHandler)
            }
        };

        self.start_with_custom_action(runtime, handler);
    }

    /// Starts the monitoring process with custom action handlers that
    /// allow you to customize what happens when blocking is detected.
    fn start_with_custom_action(
        &self,
        runtime: Arc<Runtime>,
        action: Arc<dyn BlockingActionHandler>,
    ) {
        *self.stop_flag.lock().unwrap() = false;
        let stop_flag = Arc::clone(&self.stop_flag);
        let detection_time = self.detection_time;
        let interval = self.interval;
        let workers = Arc::clone(&self.workers);
        thread::spawn(move || {
            let mut rng = rng();
            while !*stop_flag.lock().unwrap() {
                probe(&runtime, detection_time, &workers, &action);
                thread::sleep(Duration::from_micros(
                    rng.random_range(10..=interval.as_micros().try_into().unwrap()),
                ));
            }
        });
    }

    /// Stops the monitoring thread. Does nothing if monitoring thread is already stopped.
    pub fn stop(&self) {
        let mut sf = self.stop_flag.lock().unwrap();
        if !(*sf) {
            *sf = true;
        }
    }
}

impl Drop for LongRunningTaskDetector {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
pub mod unix {
    use std::cell::UnsafeCell;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicPtr;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    // ── tunables ─────────────────────────────────────────────────────────────
    /// Independent stack-trace samples taken per detection event.
    /// IPs present in *all* samples are provably stuck, not transiently in-flight.
    const NUM_SAMPLES: usize = 3;
    /// Gap between samples – long enough for any in-flight frame to disappear.
    const SAMPLE_INTERVAL_MS: u64 = 150;
    /// Wall-clock budget for all signal replies in one collection round.
    const SIGNAL_WAIT_SECS: u64 = 2;
    /// Maximum call-stack depth stored per thread per sample.
    const MAX_FRAMES: usize = 128;
    /// Minimum time between full reports for the *same* culprit symbol.
    /// Between full reports the handler prints a compact one-liner instead.
    const DEDUP_INTERVAL: Duration = Duration::from_secs(30);

    // ── internal symbol prefix filter ────────────────────────────────────────
    /// Prefixes that identify runtime / OS / language-infrastructure frames.
    /// The culprit is the first *non*-matching frame in the stuck set.
    const INTERNAL_PREFIXES: &[&str] = &[
        "std::",
        "core::",
        "alloc::",
        "tokio::",
        "mio::",
        "epoll_wait",
        "clone",
        "libc::",
        "pulsebeam_runtime::rt::detector",
        "__pthread",
        // tracing / instrumentation infrastructure – Instrumented<T>::poll is
        // always the outermost async wrapper; the *actual* application future
        // inside it is an opaque state-machine and won't appear as a distinct
        // named frame on the OS stack anyway, so filtering tracing:: here
        // keeps the culprit selection from always stopping at the wrapper.
        "tracing::",
        "futures::",
        "futures_util::",
        "futures_core::",
        "futures_lite::",
        "pin_project_lite::",
        "backtrace::",
        "rustc_demangle::",
        "addr2line::",
        "gimli::",
        "object::",
        "memchr::",
        // Our own detector machinery (signal handler and its helpers).
        "pulsebeam_runtime::rt::detector::",
        "signal_handler",
        // libc / OS stubs always start with "__".
        "__",
        // Kernel syscalls that represent *legitimate* blocking.
        // A tokio worker serving as the IO driver will be stuck in epoll_wait
        // while the system is idle; that is expected and should not be reported.
        "epoll_wait",
        "io_uring_enter",
        "kevent",
        "kqueue",
    ];

    // ── lock-free signal-handler infrastructure ──────────────────────────────
    //
    // Design goals:
    //  - Zero heap allocation inside the signal handler.
    //  - Zero mutex lock inside the signal handler (Mutex::lock can call malloc,
    //    which can deadlock if the interrupted thread already holds the allocator).
    //  - Only Release/Acquire atomics for synchronisation between the signal
    //    handler (writer) and the collector thread (reader).
    //
    // Protocol:
    //  1. Collector allocates a `CaptureSession` on the heap, fills one
    //     `SlotEntry` per target thread, publishes the pointer via
    //     `CURRENT_SESSION` (Release store), then sends SIGUSR1 to every target.
    //  2. Each target's signal handler loads the session pointer (Acquire load),
    //     finds its pre-allocated slot by scanning `slots` for its pthread_id,
    //     fills `slot.ips` via `trace_unsynchronized`, signals completion via
    //     `slot.len.store(count, Release)`, and decrements `session.remaining`.
    //  3. Collector spins on `session.remaining` until it reaches 0 (or times
    //     out), then clears `CURRENT_SESSION` (Release store), reclaims the Box,
    //     and resolves IPs → symbols on its own thread (safe, no signal context).
    //
    // Safety: each `slot.ips` is written by exactly one thread (the target whose
    // pthread_id matches).  The collector reads it only after `remaining == 0`,
    // which happens-after all `slot.len.store(Release)` writes, giving the
    // necessary happens-before for the `slot.ips` data.

    /// One pre-allocated capture buffer per target thread.
    struct SlotEntry {
        tid: libc::pthread_t,
        /// Written exclusively by the target's signal handler before len is set.
        ips: UnsafeCell<[usize; MAX_FRAMES]>,
        /// 0 until the signal handler is done; stored with Release.
        len: AtomicUsize,
    }

    // SAFETY: each slot is written by exactly one thread; read only after the
    // Release/Acquire pair on `len` establishes the happens-before.
    unsafe impl Sync for SlotEntry {}

    struct CaptureSession {
        slots: Box<[SlotEntry]>,
        remaining: AtomicUsize,
    }

    // SAFETY: published/consumed through an AtomicPtr with Release/Acquire.
    unsafe impl Sync for CaptureSession {}
    unsafe impl Send for CaptureSession {}

    /// Pointer to the live `CaptureSession`, or null when no collection is active.
    static CURRENT_SESSION: AtomicPtr<CaptureSession> = AtomicPtr::new(ptr::null_mut());

    extern "C" fn signal_handler(_: i32) {
        // Load session pointer.  If null, we are a stray delayed signal – ignore.
        let ptr = CURRENT_SESSION.load(Ordering::Acquire);
        if ptr.is_null() {
            return;
        }
        // SAFETY: the collector keeps the Box alive until after it clears
        // CURRENT_SESSION and all remaining counts have reached zero.
        let session: &CaptureSession = unsafe { &*ptr };

        let my_tid = unsafe { libc::pthread_self() };

        // Locate our pre-allocated slot (linear scan; worker count is tiny).
        let slot = match session.slots.iter().find(|s| s.tid == my_tid) {
            Some(s) => s,
            None => {
                // We were not among the intended targets; just decrement so the
                // collector does not hang.
                session.remaining.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        };

        // Walk the stack and store raw IPs into our pre-allocated buffer.
        // SAFETY: `trace_unsynchronized` is safe here because:
        //   - `force-frame-pointers = true` means we use frame-pointer unwinding,
        //     which requires no allocation and holds no locks.
        //   - GTI_MUTEX (held by the collector) ensures at most one collection
        //     round is active at a time, so no two signal handlers call
        //     `trace_unsynchronized` concurrently.
        let buf: &mut [usize; MAX_FRAMES] = unsafe { &mut *slot.ips.get() };
        let mut count = 0usize;
        unsafe {
            backtrace::trace_unsynchronized(|frame| {
                if count < MAX_FRAMES {
                    buf[count] = frame.ip() as usize;
                    count += 1;
                }
                true
            });
        }

        // Publish the frame count.  The Release store pairs with the Acquire
        // load in the collector, ensuring visibility of the `buf` writes.
        slot.len.store(count, Ordering::Release);
        session.remaining.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn install_default_stack_trace_handler() {
        unsafe {
            libc::signal(libc::SIGUSR1, signal_handler as libc::sighandler_t);
        }
    }

    // ── structured frame data ────────────────────────────────────────────────

    /// All symbols resolved from a single instruction pointer (may be > 1 due to
    /// inlining).  `ip` is kept so intersection can operate at the IP level while
    /// still surfacing every inlined symbol for display.
    #[derive(Clone, Debug)]
    struct IpFrames {
        ip: usize,
        symbols: Vec<SymbolData>,
    }

    #[derive(Clone, Debug)]
    struct SymbolData {
        name: Option<String>,
        filename: Option<String>,
        lineno: Option<u32>,
    }

    impl SymbolData {
        fn is_internal(&self) -> bool {
            match &self.name {
                None => true,
                Some(n) => {
                    // Fast path: plain symbol like `tokio::runtime::...`
                    if INTERNAL_PREFIXES.iter().any(|p| n.starts_with(p)) {
                        return true;
                    }
                    // Handle angle-bracket wrapped trait impls:
                    //   `<tokio::runtime::blocking::task::BlockingTask<T> as Future>::poll`
                    //   `<futures_lite::future::CatchUnwind<F> as Future>::poll::{{closure}}`
                    // Strip the leading `<` and check the inner type against prefixes.
                    if let Some(inner) = n.strip_prefix('<') {
                        // Inner is everything up to the first `>` or ` as `
                        let type_name = if let Some(pos) = inner.find(" as ") {
                            &inner[..pos]
                        } else if let Some(pos) = inner.find('>') {
                            &inner[..pos]
                        } else {
                            inner
                        };
                        if INTERNAL_PREFIXES.iter().any(|p| type_name.starts_with(p)) {
                            return true;
                        }
                    }
                    false
                }
            }
        }

        fn short_location(&self) -> String {
            match (&self.filename, self.lineno) {
                (Some(f), Some(l)) => {
                    let short = std::path::Path::new(f)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(f.as_str());
                    format!("{}:{}", short, l)
                }
                (Some(f), None) => f.clone(),
                _ => String::new(),
            }
        }
    }

    // ── sample collection ────────────────────────────────────────────────────

    /// Serialises concurrent collection rounds (one at a time), ensuring
    /// `trace_unsynchronized` is never called concurrently across threads in the
    /// signal handler.
    static GTI_MUTEX: Mutex<()> = Mutex::new(());

    /// Per-thread raw IPs for one collection round.
    type RawSample = HashMap<libc::pthread_t, Vec<IpFrames>>;

    /// Resolve raw instruction pointers -> `IpFrames` for one thread's capture.
    fn resolve_ips(ips: &[usize]) -> Vec<IpFrames> {
        // Detect the signal-delivery boundary so we can strip the signal
        // machinery frames from display.  The boundary is the frame immediately
        // after the OS signal trampoline (`__restore_rt` on Linux, `_sigtramp`
        // on macOS, or any `<unknown>` cluster right at the top).
        // We strip frames that clearly belong to the signal delivery path by
        // skipping the leading IPs whose only resolved symbol is our own
        // `signal_handler`, the OS trampoline, or <unknown>.
        let mut found_user_code = false;
        let mut frames: Vec<IpFrames> = Vec::with_capacity(ips.len());

        for &ip in ips {
            let mut syms: Vec<SymbolData> = Vec::new();
            backtrace::resolve(ip as *mut c_void, |sym| {
                syms.push(SymbolData {
                    name: sym.name().map(|n| n.to_string()),
                    filename: sym
                        .filename()
                        .and_then(|p| p.to_str())
                        .map(|s| s.to_owned()),
                    lineno: sym.lineno(),
                });
            });
            if syms.is_empty() {
                syms.push(SymbolData {
                    name: None,
                    filename: None,
                    lineno: None,
                });
            }

            // Strip leading signal-machinery IPs before the first user frame.
            if !found_user_code {
                let is_machinery = syms.iter().all(|s| s.is_internal());
                if is_machinery {
                    continue; // skip signal handler / trampoline / OS frames
                }
                found_user_code = true;
            }

            frames.push(IpFrames { ip, symbols: syms });
        }
        frames
    }

    /// Send `signal` to every thread in `targets`, collect raw IPs, and resolve
    /// them to `IpFrames`.  Returns one entry per thread that replied in time.
    fn collect_once(signal: libc::c_int, targets: &[ThreadInfo]) -> RawSample {
        // Serialise rounds so (a) CURRENT_SESSION is unambiguous and (b)
        // `trace_unsynchronized` is never concurrent across our signal handlers.
        let _lock = GTI_MUTEX.lock().unwrap();

        // Allocate one slot per target.
        let slots: Vec<SlotEntry> = targets
            .iter()
            .map(|ti| SlotEntry {
                tid: *ti.pthread_id(),
                ips: UnsafeCell::new([0; MAX_FRAMES]),
                len: AtomicUsize::new(0),
            })
            .collect();

        let session = Box::new(CaptureSession {
            slots: slots.into_boxed_slice(),
            remaining: AtomicUsize::new(targets.len()),
        });
        let session_ptr = Box::into_raw(session);

        // Publish the session pointer before any signal is sent.
        CURRENT_SESSION.store(session_ptr, Ordering::Release);

        for ti in targets {
            let rc = unsafe { libc::pthread_kill(*ti.pthread_id(), signal) };
            if rc != 0 {
                eprintln!("[detector] Failed to send signal to thread: errno={}", rc);
                // Prevent the collector from hanging on this thread.
                let s = unsafe { &*session_ptr };
                s.remaining.fetch_sub(1, Ordering::SeqCst);
            }
        }

        // Wait for all signal handlers to complete (or time out).
        let deadline = Instant::now() + Duration::from_secs(SIGNAL_WAIT_SECS);
        loop {
            let s = unsafe { &*session_ptr };
            if s.remaining.load(Ordering::Acquire) == 0 {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "[detector] Timed out waiting for {} signal replies",
                    unsafe { &*session_ptr }.remaining.load(Ordering::SeqCst)
                );
                break;
            }
            thread::sleep(Duration::from_micros(50));
        }

        // Disable the session BEFORE reclaiming the Box, so any late signal
        // (rogue delayed delivery) sees null and exits harmlessly.
        CURRENT_SESSION.store(ptr::null_mut(), Ordering::Release);

        // Retake ownership and resolve IPs on this (non-signal) thread.
        let session = unsafe { Box::from_raw(session_ptr) };
        let mut result = HashMap::new();
        for slot in session.slots.iter() {
            // Acquire load pairs with the Release store in the signal handler,
            // ensuring the IPs written before len.store are visible here.
            let len = slot.len.load(Ordering::Acquire);
            if len == 0 {
                continue; // thread did not reply (timed out or send failed)
            }
            let buf = unsafe { &*slot.ips.get() };
            let frames = resolve_ips(&buf[..len]);
            result.insert(slot.tid, frames);
        }
        result
    }

    /// Collect `NUM_SAMPLES` independent snapshots with `SAMPLE_INTERVAL_MS`
    /// gaps between them.
    fn collect_multi_samples(signal: libc::c_int, targets: &[ThreadInfo]) -> Vec<RawSample> {
        (0..NUM_SAMPLES)
            .map(|i| {
                let s = collect_once(signal, targets);
                if i + 1 < NUM_SAMPLES {
                    thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
                }
                s
            })
            .collect()
    }

    // ── stuck-frame analysis ─────────────────────────────────────────────────

    /// For each thread, return only the `IpFrames` whose instruction pointer
    /// appeared in **every** sample.  These are provably-blocked call sites –
    /// transient in-flight frames (which appear in only some samples) are excluded.
    ///
    /// Intersection operates on raw IPs, not symbol names, so two call sites
    /// that happen to share a demangled name are correctly distinguished.
    fn find_stuck_frames(all_samples: &[RawSample]) -> HashMap<libc::pthread_t, Vec<IpFrames>> {
        if all_samples.is_empty() {
            return HashMap::new();
        }

        let all_tids: HashSet<libc::pthread_t> =
            all_samples.iter().flat_map(|s| s.keys().copied()).collect();

        all_tids
            .into_iter()
            .filter_map(|tid| {
                let ip_sets: Vec<HashSet<usize>> = all_samples
                    .iter()
                    .filter_map(|s| s.get(&tid))
                    .map(|frames| frames.iter().map(|f| f.ip).collect())
                    .collect();

                if ip_sets.is_empty() {
                    return None;
                }

                // IPs common to every sample.
                let stuck_ips: HashSet<usize> = ip_sets[0]
                    .iter()
                    .filter(|ip| ip_sets[1..].iter().all(|s| s.contains(*ip)))
                    .copied()
                    .collect();

                if stuck_ips.is_empty() {
                    return None;
                }

                // Preserve call-site order from the last sample.
                let last = all_samples.last()?.get(&tid)?;
                let stuck: Vec<IpFrames> = last
                    .iter()
                    .filter(|f| stuck_ips.contains(&f.ip))
                    .cloned()
                    .collect();

                Some((tid, stuck))
            })
            .collect()
    }

    /// The culprit is the shallowest (most-recently-called) non-internal symbol
    /// across all inlined frames within the stuck IPs.
    ///
    /// Falls back to scanning the full last-sample trace if the stuck set is
    /// empty or contains only internal frames.
    fn identify_culprit<'a>(
        stuck: &'a [IpFrames],
        fallback: &'a [IpFrames],
    ) -> Option<&'a SymbolData> {
        let search = |frames: &'a [IpFrames]| {
            frames.iter().flat_map(|f| f.symbols.iter()).find(|s| !s.is_internal())
        };
        search(stuck).or_else(|| search(fallback))
    }

    // ── thread name lookup (Linux) ───────────────────────────────────────────

    /// Return the OS thread name for the given `pthread_t`.
    ///
    /// Uses `pthread_getname_np` — the POSIX extension that retrieves the name
    /// of *any* thread (not just the caller), given its handle.  The name is
    /// set by `pthread_setname_np` which tokio calls for its worker threads
    /// ("tokio-runtime-worker-N").  Falls back to the numeric handle when the
    /// call fails or the name is empty.
    fn thread_name(tid: libc::pthread_t) -> String {
        let mut buf = [0u8; 64];
        // SAFETY: `buf` is a valid writable buffer of the given length;
        // `pthread_getname_np` writes at most `buf.len()` bytes including NUL.
        let rc = unsafe {
            libc::pthread_getname_np(
                tid,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                let name = name.trim();
                if !name.is_empty() {
                    return name.to_owned();
                }
            }
        }
        format!("thread-{}", tid)
    }

    // ── report formatting ────────────────────────────────────────────────────

    fn format_report(
        tid: libc::pthread_t,
        full: &[IpFrames],
        stuck: &[IpFrames],
        culprit: Option<&SymbolData>,
        blocked_for: Duration,
        hit_count: u64,
    ) -> String {
        let stuck_ips: HashSet<usize> = stuck.iter().map(|f| f.ip).collect();
        let culprit_name: Option<&str> = culprit.and_then(|s| s.name.as_deref());
        let tname = thread_name(tid);

        let mut out = String::new();
        out.push_str(&format!(
            "\n╔══ BLOCKING DETECTED ══ {} ({}) ══ blocked for {:.1?} ══ #{} ══\n",
            tname, tid, blocked_for, hit_count
        ));

        match culprit {
            Some(c) => {
                let sym = c.name.as_deref().unwrap_or("<unknown>");
                let loc = c.short_location();
                if loc.is_empty() {
                    out.push_str(&format!("║  >>> CULPRIT: {}\n", sym));
                } else {
                    out.push_str(&format!("║  >>> CULPRIT: {}  ({})\n", sym, loc));
                }
            }
            None => {
                out.push_str(
                    "║  >>> CULPRIT: unable to identify \
                     (all stuck frames are runtime-internal)\n",
                );
            }
        }

        out.push_str("║\n║  Stack  (* = stuck in every sample,  <<< = culprit):\n");

        let mut frame_idx = 0usize;
        for ip_frames in full {
            let is_stuck = stuck_ips.contains(&ip_frames.ip);
            for sym in &ip_frames.symbols {
                let sym_name = sym.name.as_deref().unwrap_or("<unknown>");
                let loc = sym.short_location();
                let is_culprit = culprit_name.map(|c| c == sym_name).unwrap_or(false);

                let marker = if is_culprit {
                    "  <<< CULPRIT"
                } else if is_stuck {
                    "  *"
                } else {
                    ""
                };

                if loc.is_empty() {
                    out.push_str(&format!("║  {:>4}: {}{}\n", frame_idx, sym_name, marker));
                } else {
                    out.push_str(&format!(
                        "║  {:>4}: {}{}\n║        at {}\n",
                        frame_idx, sym_name, marker, loc
                    ));
                }
                frame_idx += 1;
            }
        }

        out.push_str(&format!("╚══ END {} ══\n", tname));
        out
    }

    // ── public handler ───────────────────────────────────────────────────────

    /// Per-culprit deduplication record.
    struct CulpritRecord {
        /// Total number of times this culprit has been seen.
        total_hits: u64,
        /// Wall-clock time the last *full* report was printed.
        last_full_report: Instant,
    }

    pub struct DetailedCaptureBlockingActionHandler {
        /// Last-captured frames per thread (keyed by pthread id), for test introspection.
        inner: Mutex<Option<HashMap<libc::pthread_t, Vec<IpFrames>>>>,
        /// Timestamp of the very first blocking detection event in a run of probe
        /// failures; reset by `blocking_resolved` when the runtime recovers.
        first_detected_at: Mutex<Option<Instant>>,
        /// Frequency table for culprit deduplication.  Key = culprit symbol name.
        culprit_counts: Mutex<HashMap<String, CulpritRecord>>,
    }

    impl DetailedCaptureBlockingActionHandler {
        pub fn new() -> Self {
            DetailedCaptureBlockingActionHandler {
                inner: Mutex::new(None),
                first_detected_at: Mutex::new(None),
                culprit_counts: Mutex::new(HashMap::new()),
            }
        }

        /// Returns `true` if any captured symbol name contains `needle`.
        /// Useful in tests.
        #[allow(dead_code)]
        pub fn contains_symbol(&self, needle: &str) -> bool {
            let guard = self.inner.lock().unwrap();
            guard
                .as_ref()
                .and_then(|map| map.values().next())
                .map(|frames| {
                    frames.iter().flat_map(|f| f.symbols.iter()).any(|s| {
                        s.name.as_deref().map(|n| n.contains(needle)).unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        }

        /// Returns the culprit symbol name from the last detection event, if any.
        #[allow(dead_code)]
        pub fn last_culprit(&self) -> Option<String> {
            let guard = self.inner.lock().unwrap();
            guard.as_ref()?.values().find_map(|frames| {
                frames
                    .iter()
                    .flat_map(|f| f.symbols.iter())
                    .find(|s| !s.is_internal())
                    .and_then(|s| s.name.clone())
            })
        }
    }

    impl BlockingActionHandler for DetailedCaptureBlockingActionHandler {
        fn blocking_resolved(&self) {
            *self.first_detected_at.lock().unwrap() = None;

            // Print a compact frequency summary so the reader can see which
            // culprits fired most during this blocking episode.
            let counts = self.culprit_counts.lock().unwrap();
            if !counts.is_empty() {
                let mut entries: Vec<(&String, u64)> =
                    counts.iter().map(|(k, v)| (k, v.total_hits)).collect();
                entries.sort_by(|a, b| b.1.cmp(&a.1));
                eprintln!("\n┌── Blocking episode summary (top culprits by hit count) ──");
                for (sym, hits) in &entries {
                    eprintln!("│  {:>5}×  {}", hits, sym);
                }
                eprintln!("└──────────────────────────────────────────────────────────");
            }
        }

        fn blocking_detected(&self, workers: &[ThreadInfo]) {
            let blocked_since = {
                let mut ts = self.first_detected_at.lock().unwrap();
                *ts.get_or_insert_with(Instant::now)
            };

            let samples = collect_multi_samples(libc::SIGUSR1, workers);
            let stuck_map = find_stuck_frames(&samples);
            let full_map: HashMap<libc::pthread_t, Vec<IpFrames>> =
                samples.into_iter().last().unwrap_or_default();

            let blocked_for = blocked_since.elapsed();

            for (&tid, full_frames) in &full_map {
                let stuck_frames = stuck_map.get(&tid).map(|v| v.as_slice()).unwrap_or(&[]);
                let culprit = identify_culprit(stuck_frames, full_frames);
                let culprit_key = culprit
                    .and_then(|s| s.name.as_deref())
                    .unwrap_or("<unknown>")
                    .to_owned();

                // Deduplication: decide whether to print a full report or a
                // one-liner for this culprit.
                let (hit_count, print_full) = {
                    let mut counts = self.culprit_counts.lock().unwrap();
                    let rec = counts.entry(culprit_key.clone()).or_insert_with(|| CulpritRecord {
                        total_hits: 0,
                        last_full_report: Instant::now() - DEDUP_INTERVAL - Duration::from_secs(1),
                    });
                    rec.total_hits += 1;
                    let should_print = rec.last_full_report.elapsed() >= DEDUP_INTERVAL;
                    if should_print {
                        rec.last_full_report = Instant::now();
                    }
                    (rec.total_hits, should_print)
                };

                if print_full {
                    let report = format_report(
                        tid,
                        full_frames,
                        stuck_frames,
                        culprit,
                        blocked_for,
                        hit_count,
                    );
                    eprintln!("{}", report);
                } else {
                    let tname = thread_name(tid);
                    eprintln!(
                        "[detector] {} ({}) still blocking ({:.1?}) – culprit: {} ({}× total, next full report in {:.0?})",
                        tname,
                        tid,
                        blocked_for,
                        culprit_key,
                        hit_count,
                        DEDUP_INTERVAL.saturating_sub({
                            self.culprit_counts.lock().unwrap()
                                .get(&culprit_key)
                                .map(|r| r.last_full_report.elapsed())
                                .unwrap_or(DEDUP_INTERVAL)
                        }),
                    );
                }
            }

            *self.inner.lock().unwrap() = Some(full_map);
        }
    }
}
