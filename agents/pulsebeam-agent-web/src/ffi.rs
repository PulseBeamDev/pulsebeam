#![allow(clippy::disallowed_types, reason = "UniFFI exposes custom browser media handles")]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaTrack(pub u64);

uniffi::custom_newtype!(WebMediaTrack, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaStream(pub u64);

uniffi::custom_newtype!(WebMediaStream, u64);
