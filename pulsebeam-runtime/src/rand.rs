pub use rand::seq::index::IndexVec;
pub use rand::{Rng as RngCore, SeedableRng};

/// A seed for the random number generator.
///
/// Cheap to copy and pass around. Follows tokio's `RngSeed` approach.
#[derive(Clone, Copy, Debug)]
pub struct RngSeed(u64);

impl RngSeed {
    /// Creates a new random seed from OS entropy.
    pub fn new() -> Self {
        let mut tmp: rand::rngs::StdRng = rand::make_rng();
        let mut bytes = [0u8; 8];
        rand::Rng::fill_bytes(&mut tmp, &mut bytes);
        Self(u64::from_le_bytes(bytes))
    }

    pub fn from_u64(seed: u64) -> Self {
        Self(seed)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl Default for RngSeed {
    fn default() -> Self {
        Self::new()
    }
}

/// The application RNG. Uses ChaCha20 (via [`rand::rngs::StdRng`]) for
/// strong statistical properties across all output sizes.
pub type Rng = rand::rngs::StdRng;

/// Produces child seeds from a parent RNG.
///
/// Owns the `Rng` directly — no heap allocation or locking.
/// Not `Clone`; use [`fork`](Self::fork) to hand an independent
/// generator to a new thread or component.
#[derive(Debug)]
pub struct RngSeedGenerator(Rng);

impl RngSeedGenerator {
    pub fn new(seed: RngSeed) -> Self {
        Self(Rng::seed_from_u64(seed.0))
    }

    /// Derive the next child seed.
    pub fn next_seed(&mut self) -> RngSeed {
        RngSeed(self.0.next_u64())
    }

    /// Derive an independent child generator (for a new thread/task).
    pub fn fork(&mut self) -> Self {
        Self::new(self.next_seed())
    }
}

/// Create a deterministic RNG from a fixed 64-bit seed.
pub fn seeded_rng(seed: u64) -> Rng {
    Rng::seed_from_u64(seed)
}

/// Select `amount` unique indices in the range `[0, len)` using the given RNG.
pub fn sample_indices<R: RngCore>(rng: &mut R, len: usize, amount: usize) -> IndexVec {
    rand::seq::index::sample(rng, len, amount)
}

/// Create an RNG seeded from the OS randomness source.
///
/// Call once at startup and derive all other RNGs from it.
pub fn os_rng() -> Rng {
    rand::make_rng()
}
