pub use rand::rngs::StdRng as Rng;
pub use rand::{RngCore, SeedableRng};

pub fn rng() -> impl rand::RngCore {
    rand::rng()
}
