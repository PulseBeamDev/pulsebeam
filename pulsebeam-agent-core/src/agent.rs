use alloc::collections::VecDeque;

use crate::{
    AgentConfig, AgentNotification, AgentSnapshot, ClientState, EventDisposition, StateError,
    context::{AgentContext, AgentEffect, AgentEvent},
};

pub struct Agent {
    cx: AgentContext,
    config: AgentConfig,
    snapshot: AgentSnapshot,
    desired: ClientState,
    notifications: VecDeque<AgentNotification>,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            cx: AgentContext::new(),
            config,
            snapshot: AgentSnapshot::default(),
            desired: ClientState::default(),
            notifications: VecDeque::new(),
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn set_state(&mut self, state: ClientState) -> Result<(), StateError> {
        state.validate(self.config.topology())?;
        self.desired = state;
        Ok(())
    }

    pub fn desired_state(&self) -> &ClientState {
        &self.desired
    }

    pub fn snapshot(&self) -> &AgentSnapshot {
        &self.snapshot
    }

    pub fn handle(&mut self, event: AgentEvent) -> EventDisposition {
        if !self.cx.accepts(&event) {
            return EventDisposition::IgnoredStale;
        }
        EventDisposition::Accepted
    }

    pub fn next_effect(&mut self) -> Option<AgentEffect> {
        self.cx.next_effect()
    }

    pub fn next_notification(&mut self) -> Option<AgentNotification> {
        self.notifications.pop_front()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        reason = "tests use direct assertions"
    )]

    use alloc::{string::String, vec};

    use crate::{
        ClientConnectionState, ConnectionIdentity, DataChannelEvent, DataChannelId, Generation,
        LocalSlotIntent, Topology, UpstreamSlot,
    };

    use super::*;

    fn config() -> AgentConfig {
        let topology = Topology::new(
            vec![UpstreamSlot::new("camera"), UpstreamSlot::new("screen")],
            7,
            3,
        )
        .unwrap();
        AgentConfig::new("https://example.test/api/v1", topology).unwrap()
    }

    #[test]
    fn state_replacement_rejects_unknown_slots() {
        let mut agent = Agent::new(config());
        let state = ClientState {
            local_slots: vec![LocalSlotIntent {
                slot: String::from("unknown"),
                audio: Default::default(),
                video: Default::default(),
            }],
            ..ClientState::default()
        };

        assert!(agent.set_state(state).is_err());
        assert_eq!(agent.desired_state(), &ClientState::default());
    }

    #[test]
    fn state_replacement_keeps_complete_valid_state() {
        let mut agent = Agent::new(config());
        let state = ClientState {
            connection: ClientConnectionState::Connected,
            identity: Some(ConnectionIdentity {
                room: String::from("room"),
                token: None,
                metadata: vec![],
            }),
            ..ClientState::default()
        };

        agent.set_state(state.clone()).unwrap();

        assert_eq!(agent.desired_state(), &state);
    }

    #[test]
    fn stale_generation_event_is_ignored() {
        let mut context = AgentContext::new();
        let generation = context.generation().unwrap();
        let mut agent = Agent::new(config());
        agent.cx = context;

        let stale = AgentEvent::DataChannel(DataChannelEvent::Opened {
            generation: Generation::new(generation.get().saturating_add(1)),
            id: DataChannelId::new(1),
        });

        assert_eq!(agent.handle(stale), EventDisposition::IgnoredStale);
    }
}
