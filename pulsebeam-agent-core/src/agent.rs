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
    use alloc::{string::String, vec::Vec};

    use crate::{
        context::{AgentEffect::Http, HttpEffect, RtcEffect, RtcEvent},
        id::Generation,
    };

    use super::*;

    struct AgentHarness {
        agent: Agent,
    }

    impl AgentHarness {
        fn new() -> Self {
            Self {
                agent: Agent::default(),
            }
        }

        fn set_state(&mut self, state: ClientState) {
            self.agent.set_state(state);
        }

        fn effect(&mut self) -> AgentEffect {
            self.agent.next_effect().expect("expected effect")
        }

        fn event(&mut self, event: AgentEvent) {
            self.agent.handle(event);
        }

        fn no_effect(&mut self) {
            assert!(self.agent.next_effect().is_none());
        }

        fn rtc_create_offer(&mut self) -> Generation {
            match self.effect() {
                AgentEffect::Rtc(RtcEffect::CreateOffer { generation }) => generation,
                effect => panic!("expected CreateOffer, got {effect:?}"),
            }
        }

        fn rtc_offer_created(&mut self, generation: Generation, offer: impl Into<String>) {
            self.event(AgentEvent::Rtc(RtcEvent::OfferCreated {
                generation,
                offer: offer.into(),
            }));
        }
    }

    #[test]
    fn create_connection() {
        let mut agent = AgentHarness::new();
        let mut state = ClientState::default();
        agent.set_state(state);

        state.connection = ClientConnectionState::Connected;
        agent.set_state(state);

        let id = agent.rtc_create_offer();
        agent.rtc_offer_created(id, "test");
    }
}
