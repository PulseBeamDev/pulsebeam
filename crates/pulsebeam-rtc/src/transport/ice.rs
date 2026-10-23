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
        proto: is::Protocol,
        source: SocketAddr,
        destination: SocketAddr,
    },
}

pub(crate) struct IceLayer {
    agent: IceAgent,
    locals: Box<[Candidate]>,
    validated: Option<(is::Protocol, SocketAddr, SocketAddr)>,
    nominated: Option<(is::Protocol, SocketAddr, SocketAddr)>,
}

impl IceLayer {
    pub(crate) fn new(
        local_credentials: IceCreds,
        local_candidates: &[Candidate],
        remote_credentials: IceCreds,
        remote_candidates: &[Candidate],
        controlling: bool,
        max_candidate_pairs: usize,
        control_tie_breaker: u64,
    ) -> Self {
        debug_assert!(
            !local_candidates.is_empty(),
            "transport validates candidates"
        );
        let mut agent = IceAgent::new(local_credentials);
        agent.set_controlling(controlling);
        agent.set_ice_lite(!controlling);
        agent.set_control_tie_breaker(control_tie_breaker);
        agent.set_max_candidate_pairs(max_candidate_pairs);
        for candidate in local_candidates {
            agent.add_local_candidate(candidate.clone());
        }
        agent.set_remote_credentials(remote_credentials);
        for candidate in remote_candidates {
            agent.add_remote_candidate(candidate.clone());
        }
        Self {
            agent,
            locals: local_candidates.to_vec().into_boxed_slice(),
            validated: None,
            nominated: None,
        }
    }

    pub(crate) fn handle_packet(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        proto: is::Protocol,
        bytes: &[u8],
    ) -> Result<bool, IceError> {
        let message = is::stun::StunMessage::parse(bytes).map_err(|_| IceError::InvalidPacket)?;
        let binding_request = message.is_binding_request();
        let packet = is::stun::StunPacket {
            proto,
            source,
            destination,
            message,
        };
        if !self.agent.handle_packet(now, packet) {
            return Err(IceError::InvalidPacket);
        }
        // The pinned ICE agent authenticates response transactions, but does
        // not validate their source tuple. Only requests prove this tuple.
        if binding_request {
            self.validated = Some((proto, destination, source));
        }
        // The pinned agent evaluates receive-driven nomination only on timeout;
        // once its reply is drained, poll_timeout can defer that work for 3 seconds.
        self.agent.handle_timeout(now);
        Ok(binding_request)
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), IceError> {
        self.agent.handle_timeout(now);
        Ok(())
    }

    pub(crate) fn poll_transmit(
        &mut self,
    ) -> Option<(is::Protocol, SocketAddr, SocketAddr, Vec<u8>)> {
        self.agent.poll_transmit().map(|packet| {
            (
                packet.proto,
                packet.source,
                packet.destination,
                packet.contents.into(),
            )
        })
    }

    pub(crate) fn poll_event(&mut self) -> Option<IceEvent> {
        for _ in 0..256 {
            let event = self.agent.poll_event()?;
            match event {
                IceAgentEvent::IceConnectionStateChange(state) => {
                    return Some(IceEvent::StateChanged(state));
                }
                IceAgentEvent::NominatedSend {
                    proto,
                    source,
                    destination,
                    ..
                } => {
                    self.nominated = Some((proto, source, destination));
                    return Some(IceEvent::Nominated {
                        proto,
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

    pub(crate) fn accepts_tuple(
        &self,
        source: SocketAddr,
        destination: SocketAddr,
        proto: is::Protocol,
    ) -> bool {
        self.has_local_candidate(destination, proto)
            && self
                .nominated
                .is_some_and(|(selected_proto, local, remote)| {
                    selected_proto == proto && local == destination && remote == source
                })
    }

    pub(crate) fn can_queue_tuple(
        &self,
        source: SocketAddr,
        destination: SocketAddr,
        proto: is::Protocol,
    ) -> bool {
        self.has_local_candidate(destination, proto)
            && self
                .validated
                .is_some_and(|(validated_proto, local, remote)| {
                    validated_proto == proto && local == destination && remote == source
                })
    }

    pub(crate) fn has_local_candidate(&self, address: SocketAddr, proto: is::Protocol) -> bool {
        self.locals
            .iter()
            .any(|candidate| candidate.addr() == address && candidate.proto() == proto)
    }

    pub(crate) fn smoothed_rtt(&self) -> Option<std::time::Duration> {
        let total = self.agent.nominated_pair_total_rtt()?;
        let responses = self.agent.nominated_pair_responses_received()?;
        u32::try_from(responses)
            .ok()
            .filter(|responses| *responses != 0)
            .and_then(|responses| total.checked_div(responses))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use is::Sha1HmacProvider;
    use std::time::Duration;

    #[test]
    fn controlled_lite_agent_only_answers_authenticated_nominated_checks() {
        let local = "192.0.2.1:5000".parse().expect("local");
        let remote = "192.0.2.2:5001".parse().expect("remote");
        let mut ice = IceLayer::new(
            IceCreds {
                ufrag: "local".into(),
                pass: "localpasswordlongenough22".into(),
            },
            &[Candidate::host(local, is::Protocol::Udp).expect("local candidate")],
            IceCreds {
                ufrag: "remote".into(),
                pass: "remotepasswordlongenough22".into(),
            },
            &[],
            false,
            128,
            1,
        );
        let now = Instant::now();
        for step in 0..20 {
            ice.handle_timeout(now + Duration::from_millis(step * 50))
                .expect("timeout");
            assert!(ice.poll_transmit().is_none(), "ICE-lite initiated a check");
        }
        let request = is::stun::StunMessage::binding_request(
            "local:remote",
            is::stun::TransId::new(),
            true,
            2,
            1_000,
            true,
        );
        let mut bytes = [0; 512];
        let length = request
            .to_bytes(
                Some(b"localpasswordlongenough22"),
                &mut bytes,
                |key, payloads| is::DefaultSha1HmacProvider.sha1_hmac(key, payloads),
            )
            .expect("signed nominated request");
        assert_eq!(
            ice.handle_packet(
                now + Duration::from_secs(1),
                remote,
                local,
                is::Protocol::Udp,
                &bytes[..length]
            ),
            Ok(true)
        );
        assert!(ice.can_queue_tuple(remote, local, is::Protocol::Udp));
        let (_, source, destination, reply) = ice.poll_transmit().expect("binding reply");
        assert_eq!((source, destination), (local, remote));
        assert!(
            !is::stun::StunMessage::parse(&reply)
                .expect("reply")
                .is_binding_request()
        );
        let mut nominated = false;
        while let Some(event) = ice.poll_event() {
            nominated |= matches!(event, IceEvent::Nominated { .. });
        }
        assert!(nominated);
        assert!(ice.accepts_tuple(remote, local, is::Protocol::Udp));
        ice.handle_timeout(now + Duration::from_secs(2))
            .expect("timeout");
        assert!(
            ice.poll_transmit().is_none(),
            "ICE-lite initiated a check after nomination"
        );
    }

    #[test]
    fn authenticated_response_transaction_does_not_validate_its_source_tuple() {
        let local = "192.0.2.1:5000".parse().expect("local");
        let remote = "192.0.2.2:5001".parse().expect("remote");
        let other = "192.0.2.3:5002".parse().expect("unrelated tuple");
        let mut ice = IceLayer::new(
            IceCreds {
                ufrag: "local".into(),
                pass: "localpasswordlongenough22".into(),
            },
            &[Candidate::host(local, is::Protocol::Udp).expect("local candidate")],
            IceCreds {
                ufrag: "remote".into(),
                pass: "remotepasswordlongenough22".into(),
            },
            &[Candidate::host(remote, is::Protocol::Udp).expect("remote candidate")],
            true,
            128,
            2,
        );
        let now = Instant::now();
        let mut transmit = None;
        for step in 0..20 {
            ice.handle_timeout(now + Duration::from_millis(step * 50))
                .expect("timeout");
            transmit = ice.poll_transmit();
            if transmit.is_some() {
                break;
            }
        }
        let (_, _, _, request) = transmit.expect("binding request");
        let transaction = is::stun::StunMessage::parse(&request)
            .expect("request")
            .trans_id();
        let response = is::stun::StunMessage::binding_reply(transaction, local);
        let mut bytes = [0; 512];
        let length = response
            .to_bytes(
                Some(b"remotepasswordlongenough22"),
                &mut bytes,
                |key, payloads| is::DefaultSha1HmacProvider.sha1_hmac(key, payloads),
            )
            .expect("signed response");
        // A matching response is accepted by the pinned agent even when its
        // source differs from the transaction's candidate pair.
        assert_eq!(
            ice.handle_packet(
                now + Duration::from_secs(1),
                other,
                local,
                is::Protocol::Udp,
                &bytes[..length]
            ),
            Ok(false)
        );
        assert!(!ice.can_queue_tuple(other, local, is::Protocol::Udp));
    }
}
