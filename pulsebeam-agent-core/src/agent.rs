use crate::{
    conn,
    context::{AgentContext, AgentEffect, AgentEvent, Effects},
    host,
    id::IdGenerator,
};

pub struct ClientState {
    pub connection: ClientConnectionState,
}

pub enum ClientConnectionState {
    Connected,
    Disconnected,
}

pub struct AgentConfig {}

pub struct Agent {
    id_generator: IdGenerator,
    effects: Effects,

    conn: conn::Connection,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            id_generator: IdGenerator::new(),
            effects: Effects::new(),

            conn: conn::Connection::new(),
        }
    }

    fn context(&mut self) -> AgentContext<'_> {
        AgentContext {
            now: host::now(),
            ids: &mut self.id_generator,
            effects: &mut self.effects,
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
