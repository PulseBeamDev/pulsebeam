use std::net::SocketAddr;

use pulsebeam_agent_core::{CoreConfig, ParticipantId};
use tokio::net::UdpSocket;

use super::driver::{AgentDriver, AgentError, AgentRunner, NativeTransport};
use super::handles::Agent;

pub struct AgentBuilder {
    participant_id: ParticipantId,
    config: CoreConfig,
    transport: NativeTransport,
}

impl AgentBuilder {
    pub fn new(participant_id: impl Into<ParticipantId>) -> Self {
        Self {
            participant_id: participant_id.into(),
            config: CoreConfig::default(),
            transport: NativeTransport::None,
        }
    }

    pub fn with_config(mut self, config: CoreConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_udp(self, socket: UdpSocket, peer: SocketAddr) -> Self {
        Self {
            transport: NativeTransport::udp(socket, peer),
            ..self
        }
    }

    pub fn with_transport(mut self, transport: NativeTransport) -> Self {
        self.transport = transport;
        self
    }

    pub fn build(self) -> Result<(Agent, AgentRunner), AgentError> {
        debug_assert!(!self.participant_id.as_str().is_empty());
        let driver = AgentDriver::new(self.config, self.transport);
        let (agent, commands) = driver.agent(self.participant_id);
        Ok((agent, AgentRunner::new(driver, commands)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_creates_transport_neutral_agent() {
        let result = AgentBuilder::new("alice").build();
        assert!(result.is_ok());
    }
}
