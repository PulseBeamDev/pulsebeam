use core::time::Duration;

use crate::effect::Effects;

pub(super) enum ConnectionEffect {
    ConnectTransport,
    DisconnectTransport,
    ScheduleReconnect { after: Duration },
    Synchronize,
}

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
    fn close(self, effects: &mut Effects<ConnectionEffect>) -> Disconnected {
        effects.emit(ConnectionEffect::DisconnectTransport);
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
    fn disconnected(self, effects: &mut Effects<ConnectionEffect>) -> Connection<ReconnectWait> {
        effects.emit(ConnectionEffect::ScheduleReconnect {
            after: Duration::from_millis(100),
        });
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
