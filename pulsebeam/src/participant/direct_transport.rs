use std::collections::VecDeque;

use pulsebeam_rtc::{
    AuthenticatedPacket, ConnectionId, DataChannelEvent, EgressDatagram, IngressPacket,
    LiveConnection, LiveConnectionError, LocalTransport, NegotiatedSession, PacketId,
    PacketProvenance, TransportEvent, TransportMetadata, TransportProtocol,
};
use pulsebeam_runtime::net::{RecvPacketBatch, Transport};
use tokio::time::Instant;

const MAX_INGRESS_PER_TICK: usize = 64;

pub struct DirectTransportConfig {
    pub connection_id: ConnectionId,
    pub session: NegotiatedSession,
    pub local: LocalTransport,
}

impl DirectTransportConfig {
    pub fn new(
        connection_id: ConnectionId,
        session: NegotiatedSession,
        local: LocalTransport,
    ) -> Self {
        Self {
            connection_id,
            session,
            local,
        }
    }
}

pub enum DirectTransportOutput {
    Transport(TransportEvent),
    Data(DataChannelEvent),
    Authenticated(AuthenticatedPacket),
}

pub struct DirectTransport {
    connection: LiveConnection,
    ingress: VecDeque<RecvPacketBatch>,
    next_packet_id: u64,
}

impl DirectTransport {
    pub fn new(
        config: DirectTransportConfig,
        now: Instant,
    ) -> Result<Self, LiveConnectionError> {
        Ok(Self {
            connection: LiveConnection::new(
                config.connection_id,
                config.session,
                config.local,
                now.into(),
            )?,
            ingress: VecDeque::with_capacity(MAX_INGRESS_PER_TICK),
            next_packet_id: 0,
        })
    }

    pub fn connection(&self) -> &LiveConnection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut LiveConnection {
        &mut self.connection
    }

    pub fn enqueue(&mut self, batch: RecvPacketBatch) {
        if self.ingress.len() >= MAX_INGRESS_PER_TICK {
            let _ = self.ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.ingress.push_back(batch);
    }

    pub fn process_ingress(&mut self, now: Instant) -> Result<usize, LiveConnectionError> {
        let mut processed = 0usize;
        while processed < MAX_INGRESS_PER_TICK {
            let Some(batch) = self.ingress.front_mut() else {
                break;
            };
            let source = batch.src;
            let destination = batch.dst;
            let protocol = match batch.transport {
                Transport::Udp(_) => TransportProtocol::Udp,
                Transport::Tcp => TransportProtocol::Tcp,
            };
            let Some(bytes) = batch.next_packet() else {
                let _ = self.ingress.pop_front();
                continue;
            };
            let provenance = PacketProvenance::new(
                now.into(),
                TransportMetadata::new(protocol, source, destination),
                PacketId::new(self.next_packet_id),
            );
            self.next_packet_id = self.next_packet_id.wrapping_add(1);
            self.connection
                .handle_datagram(now.into(), IngressPacket::new(bytes, provenance))?;
            processed = processed.saturating_add(1);
        }
        Ok(processed)
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.connection.handle_timeout(now.into());
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        self.connection.next_deadline().map(Into::into)
    }

    pub fn poll_output(&mut self) -> Option<DirectTransportOutput> {
        if let Some(event) = self.connection.poll_event() {
            return Some(DirectTransportOutput::Transport(event));
        }
        if let Some(data) = self
            .connection
            .data_association()
            .and_then(|association| association.poll_event())
        {
            return Some(DirectTransportOutput::Data(data));
        }
        self.connection
            .poll_authenticated()
            .map(DirectTransportOutput::Authenticated)
    }

    pub fn poll_egress(&mut self) -> Option<EgressDatagram> {
        self.connection.poll_egress()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use pulsebeam_rtc::{
        IceCandidate, IceCredentials, ServerTransport, negotiate,
    };

    fn config(now: Instant) -> DirectTransportConfig {
        let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
            .expect("valid local ICE credentials");
        let local = LocalTransport::generate(ice).expect("local transport");
        let candidate = IceCandidate::new(
            "candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned(),
        )
        .expect("valid ICE candidate");
        let server = ServerTransport::new(
            7,
            local.ice().clone(),
            local.fingerprint().clone(),
            Box::new([candidate]),
        );
        let offer = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=ice-ufrag:remoteufrag\r\n\
a=ice-pwd:remotepassword\r\n\
a=fingerprint:sha-256 01:02:03:04\r\n\
a=setup:actpass\r\n\
a=candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n";
        let session = negotiate(offer, &server)
            .expect("negotiated session")
            .session()
            .clone();
        let _ = now;
        DirectTransportConfig::new(ConnectionId::new(7), session, local)
    }

    fn batch(bytes: Vec<u8>) -> RecvPacketBatch {
        RecvPacketBatch {
            src: SocketAddr::from(([127, 0, 0, 1], 9001)),
            dst: SocketAddr::from(([127, 0, 0, 1], 9000)),
            len: bytes.len(),
            stride: bytes.len().max(1),
            buf: bytes,
            transport: Transport::Udp(pulsebeam_runtime::net::UdpMode::Scalar),
            offset: 0,
        }
    }

    #[test]
    fn malformed_ingress_is_rejected_without_unbounded_work() {
        let now = Instant::now();
        let mut transport = DirectTransport::new(config(now), now).expect("direct transport");
        transport.enqueue(batch(vec![0xff]));

        assert!(transport.process_ingress(now).is_err());
        assert!(transport.poll_output().is_none());
    }
}
