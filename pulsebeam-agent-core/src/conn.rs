use core::time::Duration;

use alloc::string::String;

use crate::{
    context::AgentContext,
    effect::{Effects, HttpEffect, RtcEffect},
    http::HttpRequest,
    id::Generation,
};

pub struct Connection<C: ConnectionState> {
    state: C,
}

pub(super) trait ConnectionState {}
impl ConnectionState for New {}
impl ConnectionState for Connecting {}
impl ConnectionState for Connected {}
impl ConnectionState for ReconnectWait {}
impl ConnectionState for Disconnected {}

trait Closable: Sized {}
impl Closable for New {}
impl Closable for Connecting {}
impl Closable for Connected {}
impl Closable for ReconnectWait {}
impl<C: Closable + ConnectionState> Connection<C> {
    fn close(self, cx: &mut AgentContext) -> Disconnected {
        Disconnected {
            reason: DisconnectedReason::UserInitiated,
        }
    }
}

pub(super) struct New {}

impl Connection<New> {
    pub(super) const fn new() -> Self {
        Self { state: New {} }
    }

    pub(super) fn connect(self) -> Connection<Connecting> {
        Connection {
            state: Connecting {},
        }
    }
}

pub(super) struct Connecting {}

impl Connection<Connecting> {
    fn connected(self) -> Connection<Connected> {
        Connection {
            state: Connected {},
        }
    }
}

pub(super) struct Connected {}

impl Connection<Connected> {
    fn disconnected(self, cx: &mut AgentContext) -> Connection<ReconnectWait> {
        Connection {
            state: ReconnectWait {},
        }
    }
}

pub(super) struct ReconnectWait {}

impl ReconnectWait {
    fn close(self) -> Connection<Disconnected> {
        Connection {
            state: Disconnected {
                reason: DisconnectedReason::UserInitiated,
            },
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub(super) enum DisconnectedReason {
    #[error("user initiated")]
    UserInitiated,
}

pub(super) struct Disconnected {
    reason: DisconnectedReason,
}
