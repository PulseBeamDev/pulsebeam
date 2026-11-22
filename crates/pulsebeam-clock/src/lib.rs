//! Caller-clocked media timeline estimation and time arithmetic.
#![forbid(unsafe_code)]

pub mod deadline;
pub mod domain;

#[cfg(test)]
mod deadline_tests;
pub mod ntp;

#[cfg(test)]
mod domain_tests;
pub mod time;

#[cfg(test)]
mod ntp_tests;

#[cfg(test)]
mod properties;

#[cfg(test)]
mod time_tests;

/// Failure to validate, represent, or resolve a timing result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Invalid,
    Overflow,
    WrongBinding,
    Stale,
    Ambiguous,
    OutOfWindow,
    Unresolved,
}
