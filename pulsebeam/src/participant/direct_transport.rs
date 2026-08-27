use std::collections::VecDeque;
use std::net::SocketAddr;

use pulsebeam_rtc::{
    AuthenticatedPacket, ConnectionId, DataChannelEvent, EgressDatagram, IngressPacket,
    LiveConnection, LiveConnectionError, LocalTransport, MediaError, MediaEvent, MediaForwarder,
    NegotiatedSession, PacketId, PacketProvenance, ReceiveStream, SendId, SendStream,
    TransportEvent, TransportMetadata, TransportProtocol,
};
use pulsebeam_runtime::net::{RecvPacketBatch, Transport};
use tokio::time::Instant;

use crate::rtp::{Codec, PacketExtensions, RtpPacket};

const MAX_INGRESS_PER_TICK: usize = 64;
const AUDIO_LEVEL_EXTENSION_URI: &str = "ssrc-audio-level";
const MID_EXTENSION_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
const RID_EXTENSION_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";

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
    Rtp {
        stream: ReceiveStream,
        packet: RtpPacket,
    },
    Rtcp(AuthenticatedPacket),
}

pub struct DirectTransport {
    connection: LiveConnection,
    media: MediaForwarder,
    ingress: VecDeque<RecvPacketBatch>,
    next_packet_id: u64,
    last_ingress: Option<(SocketAddr, SocketAddr)>,
}

impl DirectTransport {
    pub fn new(config: DirectTransportConfig, now: Instant) -> Result<Self, LiveConnectionError> {
        Ok(Self {
            connection: LiveConnection::new(
                config.connection_id,
                config.session,
                config.local,
                now.into(),
            )?,
            media: MediaForwarder::with_capacity(64, 64, 128),
            ingress: VecDeque::with_capacity(MAX_INGRESS_PER_TICK),
            next_packet_id: 0,
            last_ingress: None,
        })
    }

    pub fn connection(&self) -> &LiveConnection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut LiveConnection {
        &mut self.connection
    }

    pub fn register_receive(&mut self, stream: ReceiveStream) -> Result<(), MediaError> {
        self.media.register_receive(stream)
    }

    pub fn register_send(&mut self, stream: SendStream) -> Result<(), MediaError> {
        self.media.register_send(stream)
    }

    pub fn unregister_receive(&mut self, id: pulsebeam_rtc::StreamId) -> Option<ReceiveStream> {
        self.media.unregister_receive(id)
    }

    pub fn unregister_send(&mut self, id: pulsebeam_rtc::StreamId) -> Option<SendStream> {
        self.media.unregister_send(id)
    }

    pub fn poll_media_event(&mut self) -> Option<MediaEvent> {
        self.media.poll_event()
    }

    pub fn report_departure(
        &mut self,
        send_id: SendId,
        now: Instant,
    ) -> Result<(), LiveConnectionError> {
        self.connection.report_departure(send_id, now.into())
    }

    pub fn send_rtp(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
        send_id: SendId,
    ) -> Result<pulsebeam_rtc::EgressCongestion, LiveConnectionError> {
        self.connection
            .send_rtp_with_congestion(bytes, extended_sequence, send_id)
    }

    pub fn send_rtp_untracked(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
    ) -> Result<(), LiveConnectionError> {
        self.connection.send_rtp(bytes, extended_sequence)
    }

    pub fn assign_congestion(
        &mut self,
        send_id: SendId,
        bytes: usize,
    ) -> Result<pulsebeam_rtc::EgressCongestion, LiveConnectionError> {
        self.connection.assign_congestion(send_id, bytes)
    }

    pub fn send_rtp_with_assigned_congestion(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
        send_id: SendId,
    ) -> Result<(), LiveConnectionError> {
        self.connection
            .send_rtp_with_assigned_congestion(bytes, extended_sequence, send_id)
    }

    pub fn send_rtcp(&mut self, bytes: &[u8]) -> Result<(), LiveConnectionError> {
        self.connection.send_rtcp(bytes)
    }

    pub fn poll_congestion(&mut self) -> Option<pulsebeam_rtc::GccOutcome> {
        self.connection.poll_congestion()
    }

    pub fn send_data(
        &mut self,
        channel: pulsebeam_rtc::ChannelId,
        binary: bool,
        bytes: Vec<u8>,
        now: Instant,
    ) -> Result<(), pulsebeam_rtc::DataChannelError> {
        let Some(association) = self.connection.data_association() else {
            return Err(pulsebeam_rtc::DataChannelError::UnknownChannel(channel));
        };
        association.send(channel, binary, bytes)?;
        self.connection.handle_timeout(now.into());
        Ok(())
    }

    pub fn enqueue(&mut self, batch: RecvPacketBatch) {
        self.last_ingress = Some((batch.src, batch.dst));
        if self.ingress.len() >= MAX_INGRESS_PER_TICK {
            let _ = self.ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.ingress.push_back(batch);
    }

    pub fn ingress_context(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.last_ingress
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

    pub fn next_deadline(&mut self, now: Instant) -> Option<Instant> {
        let minimum = now
            .checked_add(std::time::Duration::from_millis(1))
            .unwrap_or(now);
        self.connection
            .next_deadline()
            .map(Into::into)
            .map(|deadline: Instant| deadline.max(minimum))
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
        let authenticated = self.connection.poll_authenticated()?;
        let packet = authenticated.parse().ok()?;
        match self.media.handle_authenticated(packet).ok()? {
            Some(pulsebeam_rtc::MediaIngress::Rtp { stream, packet }) => {
                let (codec, extensions) = self.rtp_metadata(stream, &packet);
                Some(DirectTransportOutput::Rtp {
                    stream,
                    packet: RtpPacket::from_packet_view(
                        &packet,
                        codec,
                        extensions,
                        Vec::with_capacity(packet.payload().len()),
                    ),
                })
            }
            Some(pulsebeam_rtc::MediaIngress::Rtcp(_)) | None => {
                Some(DirectTransportOutput::Rtcp(authenticated))
            }
        }
    }

    pub fn poll_egress(&mut self) -> Option<EgressDatagram> {
        self.connection.poll_egress()
    }

    fn rtp_metadata(
        &self,
        stream: ReceiveStream,
        packet: &pulsebeam_rtc::RtpPacketView<'_>,
    ) -> (Codec, PacketExtensions) {
        let Some(section) = self.connection.media_section(stream.media_section()) else {
            debug_assert!(
                false,
                "a registered receive stream has negotiated media facts"
            );
            return (Codec::H264, PacketExtensions::default());
        };
        let codec = section
            .codecs()
            .iter()
            .find(|codec| codec.payload_type() == packet.payload_type())
            .map_or(Codec::H264, |codec| {
                codec
                    .name()
                    .eq_ignore_ascii_case("opus")
                    .then_some(Codec::Opus)
                    .unwrap_or(Codec::H264)
            });
        let mut extensions = PacketExtensions::default();
        let audio_level = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri().contains(AUDIO_LEVEL_EXTENSION_URI))
            .and_then(|extension| {
                packet
                    .header_extension(section, extension.id())
                    .ok()
                    .flatten()
            })
            .and_then(|value| value.value().first().copied())
            .and_then(|value| i8::try_from(value & 0x7f).ok())
            .and_then(|value| value.checked_neg());
        extensions.audio_level = audio_level;
        extensions.mid = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == MID_EXTENSION_URI)
            .and_then(|extension| {
                packet
                    .header_extension(section, extension.id())
                    .ok()
                    .flatten()
            })
            .and_then(|value| std::str::from_utf8(value.value()).ok())
            .filter(|mid| !mid.is_empty())
            .map(Into::into);
        extensions.rid = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == RID_EXTENSION_URI)
            .and_then(|extension| {
                packet
                    .header_extension(section, extension.id())
                    .ok()
                    .flatten()
            })
            .and_then(|value| std::str::from_utf8(value.value()).ok())
            .filter(|rid| !rid.is_empty())
            .map(Into::into);
        (codec, extensions)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use pulsebeam_rtc::{IceCandidate, IceCredentials, ServerTransport, negotiate};

    fn config(now: Instant) -> DirectTransportConfig {
        let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
            .expect("valid local ICE credentials");
        let local = LocalTransport::generate(ice).expect("local transport");
        let candidate =
            IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
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
