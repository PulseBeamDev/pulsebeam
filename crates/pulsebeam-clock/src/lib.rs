//! Caller-clocked media timeline estimation and time arithmetic.
#![forbid(unsafe_code)]

pub mod ntp;
pub mod time;

#[cfg(test)]
mod ntp_tests;

#[cfg(test)]
mod time_tests;

/// Failure to validate, represent, or resolve a timing result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Overflow,
    WrongBinding,
    Stale,
    Ambiguous,
    OutOfWindow,
    Unresolved,
}
