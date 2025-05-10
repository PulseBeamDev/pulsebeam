#[cfg(test)]
pub use rand_chacha::ChaCha8Rng as Rng;

#[cfg(not(test))]
pub use rand::rngs::StdRng as Rng;
