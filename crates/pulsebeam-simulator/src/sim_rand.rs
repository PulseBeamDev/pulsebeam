//! Make OS randomness reproducible, so runs are comparable.
//!
//! Derived from madsim's equivalent:
//! <https://github.com/madsim-rs/madsim/blob/main/madsim/src/sim/rand.rs>
//!
//! # Why this is necessary
//!
//! `std`'s `RandomState` — which randomizes `HashMap` and `HashSet` iteration order — seeds
//! itself once per thread from `getrandom(2)`. Any dependency iterating a map therefore visits
//! entries in a different order on every run. The same applies to anything else drawing from the
//! OS, including DTLS key generation.
//!
//! Overriding the libc symbol reaches inside dependencies we do not control, which is the part a
//! change to our own types cannot do.
//!
//! # Thread-local, not global
//!
//! The upstream reference keeps one process-wide RNG, which fits a runner executing a single
//! simulation per process. Our suite runs many plans in parallel in one process, and a shared
//! RNG would let them interleave draws — each plan's byte sequence would then depend on what the
//! others happened to be doing, which is precisely the nondeterminism this removes.
//!
//! `cargo test` gives each test its own thread and turmoil drives that plan's hosts on it, so
//! thread-local state is exactly one plan's worth. The harness seeds it from the plan's own seed,
//! making each plan reproducible on its own terms.
//!
//! # Scope
//!
//! Test-binary only; this crate is never shipped. Until a thread calls [`set_thread_rng`] it
//! falls through to the real OS, so the test runner's own start-up still gets real entropy.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read};

use rand::SeedableRng;
use rand::rngs::StdRng;

thread_local! {
    static RNG: RefCell<Option<StdRng>> = const { RefCell::new(None) };
}

/// Seed this thread's randomness. Call once at the start of a plan.
pub fn set_thread_rng(seed: u64) {
    RNG.with(|cell| *cell.borrow_mut() = Some(StdRng::seed_from_u64(seed)));
}

/// Clear this thread's randomness, restoring real OS entropy.
#[allow(dead_code)]
pub fn clear_thread_rng() {
    RNG.with(|cell| *cell.borrow_mut() = None);
}

fn fill_with_dev_urandom(dest: &mut [u8]) -> io::Result<()> {
    let mut file = File::open("/dev/urandom")?;
    file.read_exact(dest)
}

/// # Safety
///
/// Overrides the libc symbol for the whole process. `buf` must be valid for writes of `buflen`
/// bytes, per the `getrandom(2)` contract.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, _flags: u32) -> isize {
    if buf.is_null() || buflen == 0 {
        return -1;
    }
    let dest = unsafe { std::slice::from_raw_parts_mut(buf, buflen) };

    // `try_with` rather than `with`: this can be reached during thread teardown, once the
    // thread-local has been destroyed, and panicking across an `extern "C"` boundary would abort.
    let served = RNG
        .try_with(|cell| {
            let mut slot = cell.borrow_mut();
            match slot.as_mut() {
                Some(rng) => {
                    rand::Rng::fill_bytes(rng, dest);
                    true
                }
                None => false,
            }
        })
        .unwrap_or(false);

    if !served && fill_with_dev_urandom(dest).is_err() {
        return -1;
    }
    isize::try_from(buflen).unwrap_or(isize::MAX)
}

/// # Safety
///
/// Overrides the libc symbol for the whole process. `buf` must be valid for writes of `buflen`
/// bytes, per the `getentropy(3)` contract.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn getentropy(buf: *mut u8, buflen: usize) -> i32 {
    if buflen > 256 {
        return -1;
    }
    match unsafe { getrandom(buf, buflen, 0) } {
        -1 => -1,
        _ => 0,
    }
}
