pub use rand::rngs::StdRng as Rng;
pub use rand::{RngCore, SeedableRng, TryRngCore};

pub use rand::seq::index::IndexVec;

/// Create a deterministic RNG from a fixed 64-bit seed.
///
/// This is useful for tests where we want repeatable behavior while still
/// allowing harnesses (e.g., Proptest) to vary the seed.
pub fn seeded_rng(seed: u64) -> Rng {
    Rng::seed_from_u64(seed)
}

/// Select `amount` unique indices in the range `[0, len)` using the given RNG.
///
/// This is useful for deterministic sampling in tests and benchmarks.
pub fn sample_indices<R: RngCore>(rng: &mut R, len: usize, amount: usize) -> IndexVec {
    rand::seq::index::sample(rng, len, amount)
}

/// Create an RNG seeded from the OS randomness source.
///
/// This is the recommended method for production use.
pub fn os_rng() -> Rng {
    // Seed from OS-provided randomness in a deterministic way.
    // This avoids using any hidden global RNG state.
    let mut seed = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut seed)
        .expect("OsRng should always be available");
    Rng::from_seed(seed)
}
