//! The only sanctioned way to end the process.
//!
//! Crashing is denied across the workspace: `unwrap`, `expect`, `panic!`,
//! `unreachable!`, `todo!` and `exit` are all denied in `[workspace.lints]`.
//! That is not style. These crates build an SFU with `panic = "abort"`, so any
//! one of them takes down every participant on the node rather than the one
//! request that hit it — a panic in an HTTP handler drops calls that had
//! nothing to do with it.
//!
//! So ending the process has to be a decision somebody wrote down, and
//! [`fatal!`](crate::fatal) is where it gets written down. It carries the
//! `#[allow]` internally, so a new crash site cannot appear by accident:
//! reaching for one either goes through here or fails to compile.
//!
//! # When this is the right answer
//!
//! Only when continuing is worse than stopping — when the process has lost
//! track of state it cannot rebuild, so carrying on means serving wrong answers
//! rather than none. Two shapes qualify:
//!
//! - **Startup.** A node that cannot bind its socket, build its runtime or
//!   register its signal handlers has nothing to degrade *to*.
//! - **Diverged state.** A shard filling the control queue it cannot block on:
//!   the node's view of the topology is already wrong and every later routing
//!   decision is made on stale data.
//!
//! Anything recoverable is not this. Dropping a malformed packet, refusing a
//! request, returning a 500, carrying on with one participant fewer — those all
//! keep the other calls up, and all of them are preferable.

/// End the process, deliberately. See the module docs.
///
/// Takes the same arguments as `panic!`. Say which invariant broke and what an
/// operator should look at; this message is the only thing that survives.
#[macro_export]
macro_rules! fatal {
    ($($arg:tt)+) => {{
        #[allow(
            clippy::panic,
            reason = "the sanctioned crash point; see pulsebeam_runtime::fatal"
        )]
        {
            panic!($($arg)+)
        }
    }};
}
