//! This module defines cross-cutting tracing targets.

use crate::entity::ConnectionId;

pub(crate) const TARGET_VIDEO: &str = "pulsebeam::x::video";
pub(crate) const TARGET_AUDIO: &str = "pulsebeam::x::audio";

#[derive(Clone, Copy)]
pub(crate) struct LogCtx {
    pub connection_id: ConnectionId,
}

macro_rules! plog {
    ($level:ident, $ctx:expr, target: $target:expr, $($rest:tt)*) => {
        ::tracing::$level!(
            target: $target,
            connection_id = %$ctx.connection_id,
            $($rest)*
        )
    };
    ($level:ident, $ctx:expr, $($rest:tt)*) => {
        ::tracing::$level!(
            connection_id = %$ctx.connection_id,
            $($rest)*
        )
    };
}

macro_rules! plog_error {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(error, $ctx, $($rest)*) };
}
macro_rules! plog_warn {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(warn, $ctx, $($rest)*) };
}
macro_rules! plog_info {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(info, $ctx, $($rest)*) };
}
macro_rules! plog_debug {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(debug, $ctx, $($rest)*) };
}
macro_rules! plog_trace {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(trace, $ctx, $($rest)*) };
}

pub(crate) use {plog, plog_debug, plog_error, plog_info, plog_trace, plog_warn};
