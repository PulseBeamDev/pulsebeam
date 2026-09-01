use std::net::SocketAddr;
use std::time::Instant;

use is::{Candidate, IceAgent, IceAgentEvent, IceConnectionState, IceCreds};

#[derive(Debug, PartialEq, Eq)]
pub enum IceError {
    InvalidPacket,
    InvalidState,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IceEvent {
    StateChanged(IceConnectionState),
    Restart,
    Nominated {
        source: SocketAddr,
        destination: SocketAddr,
    },
}

pub(crate) struct IceLayer {
    agent: IceAgent,
    local: SocketAddr,
    validated: Option<(SocketAddr, SocketAddr)>,
    nominated: Option<(SocketAddr, SocketAddr)>,
}

impl IceLayer {
    pub(crate) fn new(
        local_credentials: IceCreds,
        local_candidate: Candidate,
        remote_credentials: IceCreds,
        remote_candidates: &[Candidate],
        controlling: bool,
        max_candidate_pairs: usize,
        control_tie_breaker: u64,
    ) -> Self {
        let local = local_candidate.addr();
        let mut agent = IceAgent::new(local_credentials);
        agent.set_controlling(controlling);
        agent.set_control_tie_breaker(control_tie_breaker);
        agent.set_max_candidate_pairs(max_candidate_pairs);
        agent.add_local_candidate(local_candidate);
        agent.set_remote_credentials(remote_credentials);
        for candidate in remote_candidates {
            agent.add_remote_candidate(candidate.clone());
        }
        Self {
            agent,
            local,
            validated: None,
            nominated: None,
        }
    }

    pub(crate) fn handle_packet(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), IceError> {
        let message = is::stun::StunMessage::parse(bytes).map_err(|_| IceError::InvalidPacket)?;
        let packet = is::stun::StunPacket {
            proto: is::Protocol::Udp,
            source,
            destination,
            message,
        };
        if !self.agent.handle_packet(now, packet) {
            return Err(IceError::InvalidPacket);
        }
        self.validated = Some((destination, source));
        Ok(())
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), IceError> {
        self.agent.handle_timeout(now);
        Ok(())
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<(SocketAddr, SocketAddr, Vec<u8>)> {
        self.agent
            .poll_transmit()
            .map(|packet| (packet.source, packet.destination, packet.contents.into()))
    }

    pub(crate) fn poll_event(&mut self) -> Option<IceEvent> {
        for _ in 0..256 {
            let event = self.agent.poll_event()?;
            match event {
                IceAgentEvent::IceConnectionStateChange(state) => {
                    return Some(IceEvent::StateChanged(state));
                }
                IceAgentEvent::NominatedSend {
                    source,
                    destination,
                    ..
                } => {
                    self.nominated = Some((source, destination));
                    return Some(IceEvent::Nominated {
                        source,
                        destination,
                    });
                }
                IceAgentEvent::IceRestart(_) => return Some(IceEvent::Restart),
                IceAgentEvent::DiscoveredRecv { .. } => {}
            }
        }
        None
    }

    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        self.agent.poll_timeout()
    }

    pub(crate) fn accepts_tuple(&self, source: SocketAddr, destination: SocketAddr) -> bool {
        destination == self.local
            && self
                .nominated
                .is_some_and(|(local, remote)| local == destination && remote == source)
    }

    pub(crate) fn can_queue_tuple(&self, source: SocketAddr, destination: SocketAddr) -> bool {
        destination == self.local
            && self
                .validated
                .is_some_and(|(local, remote)| local == destination && remote == source)
    }
}
