use alloc::string::String;

use crate::{
    conn::{self, Connection, ConnectionState},
    effect::{HttpEffect, RtcEffect, TimerEffect},
    http::HttpResponse,
    id::{Generation, OperationId, RequestId},
};

pub struct ClientState {
    pub connection: ClientConnectionState,
}

pub enum ClientConnectionState {
    Connected,
    Disconnected,
}

pub enum AgentEvent {
    Rtc(RtcEvent),
    Http(HttpEvent),
    Timer(TimerEvent),
}

pub enum RtcEvent {
    OfferCreated {
        generation: Generation,
        offer: String,
    },

    AnswerApplied {
        generation: Generation,
    },

    Connected {
        generation: Generation,
    },

    Disconnected {
        generation: Generation,
    },
}

pub enum HttpEvent {
    Response {
        id: RequestId,
        response: HttpResponse,
    },

    Failed {
        id: RequestId,
    },
}

pub enum TimerEvent {
    Fired { id: OperationId },
}

pub enum AgentEffect {
    Rtc(RtcEffect),
    Http(HttpEffect),
    Timer(TimerEffect),
}

pub struct AgentConfig {}

pub struct Agent {
    state: ErasedAgentState,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            state: ErasedAgentState::new(),
        }
    }

    /// Replace complete desired state.
    pub fn set_state(&mut self, state: ClientState) {}

    /// Tell the engine something happened in the outside world.
    pub fn handle(&mut self, event: AgentEvent) {}

    /// Ask what outside work needs to happen next.
    pub fn next_effect(&mut self) -> Option<AgentEffect> {
        None
    }
}

pub enum ErasedAgentState {
    New(AgentState<conn::New>),
    Ready(AgentState<conn::Connected>),
}

impl ErasedAgentState {
    fn new() -> Self {
        Self::New(AgentState::new())
    }
}

pub struct AgentState<C: ConnectionState> {
    conn: Connection<C>,
}

impl AgentState<conn::New> {
    fn new() -> Self {
        Self {
            conn: Connection::new(),
        }
    }

    fn connect(self) -> AgentState<conn::Connecting> {
        AgentState {
            conn: self.conn.connect(),
        }
    }
}
