//! Shared transport-free authentication keys and identity codecs.
//!
//! Token generation belongs to the server SDK. Registry matching and token
//! verification belong to core; neither is part of this crate.

/// Maximum complete encoded participant JWT size, including separators and signature.
pub const MAX_COMPACT_TOKEN_LEN: usize = 8_192;

/// Maximum UTF-8 byte length of an identifier, publication label, or room pattern.
pub const MAX_SCOPE_BYTES: usize = 256;

pub mod api_key;
pub mod grants;
pub mod identity;
pub mod keys;
