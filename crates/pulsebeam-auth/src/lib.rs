//! Shared transport-free authentication keys and identity codecs.
//!
//! Token generation belongs to the server SDK. Registry matching and token
//! verification belong to core; neither is part of this crate.

pub mod identity;
pub mod keys;
