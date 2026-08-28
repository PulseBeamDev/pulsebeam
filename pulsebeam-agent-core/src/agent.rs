use crate::{
    conn,
    context::{AgentContext, AgentEffect, AgentEvent},
};

#[derive(Clone, Copy)]
pub struct ClientState {
    pub connection: ClientConnectionState,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            connection: ClientConnectionState::Disconnected,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClientConnectionState {
    Connected,
    Disconnected,
}

pub struct AgentConfig {}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {}
    }
}

pub struct Agent {
    cx: AgentContext,
    conn: conn::Connection,

    current: ClientState,
    desired: ClientState,
}

impl Default for Agent {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        let desired = ClientState::default();
        Self {
            cx: AgentContext::new(),
            conn: conn::Connection::new(),

            current: desired,
            desired,
        }
    }

    /// Replace complete desired state.
    pub fn set_state(&mut self, state: ClientState) {
        self.desired = state;
        self.reconcile();
    }

    /// Tell the engine something happened in the outside world.
    pub fn handle(&mut self, event: AgentEvent) {
        self.conn.handle(&event, &mut self.cx);
    }

    /// Ask what outside work needs to happen next.
    pub fn next_effect(&mut self) -> Option<AgentEffect> {
        self.cx.next_effect()
    }

    fn reconcile(&mut self) {
        self.conn.reconcile(&self.desired.connection, &mut self.cx);

        if self.conn.reached(&self.desired.connection) {
            self.current.connection = self.desired.connection;
        }
    }
}

#[cfg(test)]
mod test {
    use alloc::vec::Vec;

    use crate::context::RtcEffect;

    use super::*;

    fn collect_effects(agent: &mut Agent) -> Vec<AgentEffect> {
        let mut effects = Vec::new();
        while let Some(ef) = agent.next_effect() {
            effects.push(ef);
        }
        effects
    }

    #[test]
    fn create_connection() {
        let mut agent = Agent::default();
        let mut state = ClientState::default();

        agent.set_state(state);

        state.connection = ClientConnectionState::Connected;
        agent.set_state(state);

        assert!(
            collect_effects(&mut agent)
                .iter()
                .any(|e| matches!(e, AgentEffect::Rtc(RtcEffect::CreateOffer { .. })))
        );
    }
}
