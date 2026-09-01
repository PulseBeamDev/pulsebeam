use alloc::{string::String, vec::Vec};

use crate::{
    ClientConnectionState,
    conn::DisconnectedReason::UserInitiated,
    context::{AgentContext, AgentEvent},
    http::{HttpRequest, HttpResponse},
    id::Generation,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    New(New),
    WaitingOffer(WaitingOffer),
    Connecting(Connecting),
    Connected(Connected),
    Disconnected(Disconnected),
}

pub struct Connection {
    state: State,
}

impl Connection {
    pub(super) fn new() -> Self {
        Self {
            state: State::New(New {}),
        }
    }

    pub(super) fn reconcile(&mut self, desired: &ClientConnectionState, cx: &mut AgentContext) {
        let new_state = match (self.state, desired) {
            (State::New(s), ClientConnectionState::Connected) => State::WaitingOffer(s.connect(cx)),
            (State::WaitingOffer(s), ClientConnectionState::Disconnected) => {
                State::Disconnected(s.close(cx))
            }
            (State::Connecting(s), ClientConnectionState::Disconnected) => {
                State::Disconnected(s.close(cx))
            }
            (s, _) => s,
        };

        self.state = new_state;
    }

    pub(super) fn reached(&self, desired: &ClientConnectionState) -> bool {
        match desired {
            ClientConnectionState::Connected => {
                matches!(self.state, State::Connected(_))
            }

            ClientConnectionState::Disconnected => {
                matches!(self.state, State::Disconnected(_))
            }
        }
    }

    pub(super) fn handle(&mut self, _ev: &AgentEvent, _cx: &mut AgentContext) {}
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct New {}

impl New {
    pub(super) const fn new() -> Self {
        Self {}
    }

    pub(super) fn connect(self, cx: &mut AgentContext) -> WaitingOffer {
        let generation = cx.rtc_open();
        WaitingOffer { generation }
    }

    pub(super) fn close(self, cx: &mut AgentContext) -> Disconnected {
        cleanup(None, None, cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct WaitingOffer {
    generation: Generation,
}

impl WaitingOffer {
    pub(super) fn offer(self, _offer: String, _cx: &mut AgentContext) -> Connecting {
        Connecting {
            generation: self.generation,
        }
    }

    pub(super) fn close(self, cx: &mut AgentContext) -> Disconnected {
        cleanup(None, Some(self.generation), cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Connecting {
    generation: Generation,
}

impl Connecting {
    fn connected(self, _resp: HttpResponse, _cx: &mut AgentContext) -> Connected {
        todo!("parse HttpResponse");
        Connected {
            generation: self.generation,
        }
    }

    pub(super) fn close(self, cx: &mut AgentContext) -> Disconnected {
        cleanup(None, Some(self.generation), cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Connected {
    generation: Generation,
}

impl Connected {
    fn disconnected(self, _cx: &mut AgentContext) -> ReconnectWait {
        ReconnectWait {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ReconnectWait {}

impl ReconnectWait {
    fn close(self) -> Disconnected {
        Disconnected {
            reason: DisconnectedReason::UserInitiated,
        }
    }
}

#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DisconnectedReason {
    #[error("user initiated")]
    UserInitiated,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Disconnected {
    reason: DisconnectedReason,
}

fn cleanup(
    resource_uri: Option<String>,
    generation: Option<Generation>,
    cx: &mut AgentContext,
) -> Disconnected {
    if let Some(resource_uri) = resource_uri {
        cx.http_request(HttpRequest {
            uri: resource_uri,
            method: crate::http::HttpMethod::Delete,
            body: Vec::new(),
            headers: Vec::new(),
        });
    }

    if let Some(generation) = generation {
        cx.rtc_close(generation);
    }

    Disconnected {
        reason: UserInitiated,
    }
}
