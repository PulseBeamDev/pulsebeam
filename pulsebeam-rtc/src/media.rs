use std::collections::{HashMap, VecDeque};

use crate::{
    CompoundRtcpView, MediaSectionId, NegotiatedMediaSection, PacketError, PacketView,
    RtpPacketView, StreamId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveStream {
    id: StreamId,
    media_section: MediaSectionId,
    ssrc: u32,
}

impl ReceiveStream {
    pub const fn new(id: StreamId, media_section: MediaSectionId, ssrc: u32) -> Self {
        Self {
            id,
            media_section,
            ssrc,
        }
    }

    pub const fn id(self) -> StreamId {
        self.id
    }

    pub const fn media_section(self) -> MediaSectionId {
        self.media_section
    }

    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendStream {
    id: StreamId,
    media_section: MediaSectionId,
    ssrc: u32,
    next_sequence: u16,
    timestamp_offset: u32,
}

impl SendStream {
    pub const fn new(
        id: StreamId,
        media_section: MediaSectionId,
        ssrc: u32,
        next_sequence: u16,
        timestamp_offset: u32,
    ) -> Self {
        Self {
            id,
            media_section,
            ssrc,
            next_sequence,
            timestamp_offset,
        }
    }

    pub const fn id(self) -> StreamId {
        self.id
    }

    pub const fn media_section(self) -> MediaSectionId {
        self.media_section
    }

    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionRewrite<'a> {
    source_id: u8,
    destination_id: u8,
    value: Option<&'a [u8]>,
}

impl<'a> ExtensionRewrite<'a> {
    pub const fn new(source_id: u8, destination_id: u8, value: Option<&'a [u8]>) -> Self {
        Self {
            source_id,
            destination_id,
            value,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardedRtp {
    bytes: Vec<u8>,
    extended_sequence: u64,
    stream_id: StreamId,
}

impl ForwardedRtp {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn extended_sequence(&self) -> u64 {
        self.extended_sequence
    }

    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaEvent {
    StreamDiscovered { ssrc: u32, payload_type: u8 },
    SenderReport { ssrc: u32 },
    Feedback { packet_type: u8, format: u8 },
    KeyframeRequest { ssrc: u32 },
}

#[derive(Clone, Debug)]
pub enum MediaIngress<'a> {
    Rtp {
        stream: ReceiveStream,
        packet: RtpPacketView<'a>,
    },
    Rtcp(CompoundRtcpView<'a>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("receive stream {0:?} already exists")]
    DuplicateReceiveStream(StreamId),
    #[error("send stream {0:?} already exists")]
    DuplicateSendStream(StreamId),
    #[error("SSRC {0} already belongs to a receive stream")]
    DuplicateReceiveSsrc(u32),
    #[error("unknown receive SSRC {0}")]
    UnknownReceiveSsrc(u32),
    #[error("unknown send stream {0:?}")]
    UnknownSendStream(StreamId),
    #[error("send stream {stream:?} does not belong to media section {section:?}")]
    MediaSectionMismatch {
        stream: StreamId,
        section: MediaSectionId,
    },
    #[error("packet rewrite failed: {0}")]
    Packet(#[from] PacketError),
}

pub struct MediaForwarder {
    receives_by_ssrc: HashMap<u32, ReceiveStream>,
    receives_by_id: HashMap<StreamId, u32>,
    sends_by_id: HashMap<StreamId, SendStream>,
    events: VecDeque<MediaEvent>,
    event_capacity: usize,
}

impl MediaForwarder {
    pub fn with_capacity(
        receive_capacity: usize,
        send_capacity: usize,
        event_capacity: usize,
    ) -> Self {
        Self {
            receives_by_ssrc: HashMap::with_capacity(receive_capacity),
            receives_by_id: HashMap::with_capacity(receive_capacity),
            sends_by_id: HashMap::with_capacity(send_capacity),
            events: VecDeque::with_capacity(event_capacity),
            event_capacity,
        }
    }

    pub fn register_receive(&mut self, stream: ReceiveStream) -> Result<(), MediaError> {
        if self.receives_by_id.contains_key(&stream.id()) {
            return Err(MediaError::DuplicateReceiveStream(stream.id()));
        }
        if self.receives_by_ssrc.contains_key(&stream.ssrc()) {
            return Err(MediaError::DuplicateReceiveSsrc(stream.ssrc()));
        }
        self.receives_by_id.insert(stream.id(), stream.ssrc());
        self.receives_by_ssrc.insert(stream.ssrc(), stream);
        Ok(())
    }

    pub fn unregister_receive(&mut self, id: StreamId) -> Option<ReceiveStream> {
        let ssrc = self.receives_by_id.remove(&id)?;
        self.receives_by_ssrc.remove(&ssrc)
    }

    pub fn register_send(&mut self, stream: SendStream) -> Result<(), MediaError> {
        if self.sends_by_id.contains_key(&stream.id()) {
            return Err(MediaError::DuplicateSendStream(stream.id()));
        }
        self.sends_by_id.insert(stream.id(), stream);
        Ok(())
    }

    pub fn unregister_send(&mut self, id: StreamId) -> Option<SendStream> {
        self.sends_by_id.remove(&id)
    }

    pub fn handle_authenticated<'a>(
        &mut self,
        packet: PacketView<'a>,
    ) -> Result<Option<MediaIngress<'a>>, MediaError> {
        match packet {
            PacketView::Rtp(packet) => {
                let Some(stream) = self.receives_by_ssrc.get(&packet.ssrc()).copied() else {
                    self.push_event(MediaEvent::StreamDiscovered {
                        ssrc: packet.ssrc(),
                        payload_type: packet.payload_type(),
                    });
                    return Ok(None);
                };
                Ok(Some(MediaIngress::Rtp { stream, packet }))
            }
            PacketView::Rtcp(packet) => {
                self.handle_rtcp(&packet);
                Ok(Some(MediaIngress::Rtcp(packet)))
            }
        }
    }

    pub fn forward_rtp(
        &mut self,
        packet: &RtpPacketView<'_>,
        section: &NegotiatedMediaSection,
        destination: StreamId,
        extensions: &[ExtensionRewrite<'_>],
    ) -> Result<ForwardedRtp, MediaError> {
        let send = self
            .sends_by_id
            .get_mut(&destination)
            .ok_or(MediaError::UnknownSendStream(destination))?;
        if send.media_section() != section.id() {
            return Err(MediaError::MediaSectionMismatch {
                stream: destination,
                section: section.id(),
            });
        }
        let sequence = send.next_sequence;
        send.next_sequence = send.next_sequence.wrapping_add(1);
        let timestamp = packet.timestamp().wrapping_add(send.timestamp_offset);
        let mut bytes = Vec::with_capacity(packet.bytes().len());
        bytes.extend_from_slice(packet.bytes());
        rewrite_fixed_header(&mut bytes, sequence, timestamp, send.ssrc())?;
        for extension in extensions {
            packet.rewrite_header_extension(
                &mut bytes,
                section,
                extension.source_id,
                extension.destination_id,
                extension.value,
            )?;
        }
        Ok(ForwardedRtp {
            bytes,
            extended_sequence: u64::from(sequence),
            stream_id: destination,
        })
    }

    pub fn poll_event(&mut self) -> Option<MediaEvent> {
        self.events.pop_front()
    }

    pub fn receive_capacity(&self) -> usize {
        self.receives_by_ssrc.capacity()
    }

    pub fn send_capacity(&self) -> usize {
        self.sends_by_id.capacity()
    }

    fn handle_rtcp(&mut self, packet: &CompoundRtcpView<'_>) {
        for item in packet.packets() {
            let bytes = item.bytes();
            let ssrc = bytes
                .get(4..8)
                .map(|value| {
                    debug_assert_eq!(value.len(), 4);
                    u32::from_be_bytes([value[0], value[1], value[2], value[3]])
                })
                .unwrap_or_default();
            let packet_type = item.packet_type();
            let format = item.report_count();
            match packet_type {
                200 => self.push_event(MediaEvent::SenderReport { ssrc }),
                205 => self.push_event(MediaEvent::Feedback {
                    packet_type: 205,
                    format,
                }),
                206 => {
                    if matches!(format, 1 | 4) {
                        self.push_event(MediaEvent::KeyframeRequest { ssrc });
                    } else {
                        self.push_event(MediaEvent::Feedback {
                            packet_type: 206,
                            format,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn push_event(&mut self, event: MediaEvent) {
        if self.events.len() < self.event_capacity {
            self.events.push_back(event);
        }
    }
}

fn rewrite_fixed_header(
    bytes: &mut [u8],
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
) -> Result<(), PacketError> {
    let header = bytes.get_mut(..12).ok_or(PacketError::Truncated)?;
    debug_assert_eq!(header.len(), 12);
    header[2..4].copy_from_slice(&sequence.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Instant};

    use super::*;
    use crate::{IngressPacket, PacketId, PacketProvenance, TransportMetadata, TransportProtocol};

    fn packet(bytes: &[u8]) -> PacketView<'_> {
        let source = SocketAddr::from(([127, 0, 0, 1], 5000));
        let destination = SocketAddr::from(([127, 0, 0, 1], 6000));
        IngressPacket::new(
            bytes,
            PacketProvenance::new(
                Instant::now(),
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(1),
            ),
        )
        .parse()
        .expect("packet")
    }

    #[test]
    fn media_forwarding_ignores_inactive_receive_capacity() {
        let mut forwarder = MediaForwarder::with_capacity(4096, 4096, 8);
        let receive_capacity = forwarder.receive_capacity();
        forwarder
            .register_receive(ReceiveStream::new(
                StreamId::new(7),
                MediaSectionId::new(0),
                9,
            ))
            .expect("stream");
        let packet = packet(&[0x80, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 9]);

        let ingress = forwarder
            .handle_authenticated(packet)
            .expect("packet handling")
            .expect("known stream");

        assert!(
            matches!(ingress, MediaIngress::Rtp { stream, .. } if stream.id() == StreamId::new(7))
        );
        assert_eq!(forwarder.receive_capacity(), receive_capacity);
    }

    #[test]
    fn media_forwarding_rewrites_one_parsed_packet_for_multiple_destinations() {
        let mut forwarder = MediaForwarder::with_capacity(1, 2, 4);
        forwarder
            .register_send(SendStream::new(
                StreamId::new(8),
                MediaSectionId::new(0),
                20,
                30,
                40,
            ))
            .expect("first destination");
        forwarder
            .register_send(SendStream::new(
                StreamId::new(9),
                MediaSectionId::new(0),
                21,
                50,
                60,
            ))
            .expect("second destination");
        let packet = packet(&[0x80, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 10]);
        let PacketView::Rtp(packet) = packet else {
            panic!("RTP packet");
        };
        let section = NegotiatedMediaSection::new(
            MediaSectionId::new(0),
            "0".to_owned(),
            crate::MediaKind::Audio,
            crate::MediaDirection::ReceiveOnly,
            Box::new([]),
            Box::new([]),
            None,
        );

        let first = forwarder
            .forward_rtp(&packet, &section, StreamId::new(8), &[])
            .expect("first rewrite");
        let second = forwarder
            .forward_rtp(&packet, &section, StreamId::new(9), &[])
            .expect("second rewrite");

        assert_eq!(&first.bytes()[2..4], &30_u16.to_be_bytes());
        assert_eq!(&first.bytes()[4..8], &49_u32.to_be_bytes());
        assert_eq!(&first.bytes()[8..12], &20_u32.to_be_bytes());
        assert_eq!(&second.bytes()[2..4], &50_u16.to_be_bytes());
        assert_eq!(&second.bytes()[4..8], &69_u32.to_be_bytes());
        assert_eq!(&second.bytes()[8..12], &21_u32.to_be_bytes());
    }

    #[test]
    fn media_forwarding_does_not_materialize_unknown_ssrcs() {
        let mut forwarder = MediaForwarder::with_capacity(2, 1, 1);
        let receive_capacity = forwarder.receive_capacity();
        let packet = packet(&[0x80, 111, 0, 7, 0, 0, 0, 9, 0xff, 0xff, 0xff, 0xff]);

        let ingress = forwarder
            .handle_authenticated(packet)
            .expect("packet handling");

        assert!(ingress.is_none());
        assert_eq!(forwarder.receive_capacity(), receive_capacity);
        assert_eq!(
            forwarder.poll_event(),
            Some(MediaEvent::StreamDiscovered {
                ssrc: u32::MAX,
                payload_type: 111,
            })
        );
    }

    #[test]
    fn media_forwarding_reports_rtcp_control_events() {
        let mut forwarder = MediaForwarder::with_capacity(1, 1, 4);
        let packet = packet(&[0x80, 200, 0, 1, 0, 0, 0, 9, 0x81, 206, 0, 1, 0, 0, 0, 10]);

        let ingress = forwarder
            .handle_authenticated(packet)
            .expect("packet handling");

        assert!(matches!(ingress, Some(MediaIngress::Rtcp(_))));
        assert_eq!(
            forwarder.poll_event(),
            Some(MediaEvent::SenderReport { ssrc: 9 })
        );
        assert_eq!(
            forwarder.poll_event(),
            Some(MediaEvent::KeyframeRequest { ssrc: 10 })
        );
    }

    #[test]
    fn media_forwarding_rewrites_negotiated_extensions_in_the_copied_packet() {
        let mut forwarder = MediaForwarder::with_capacity(1, 1, 1);
        forwarder
            .register_send(SendStream::new(
                StreamId::new(8),
                MediaSectionId::new(0),
                20,
                30,
                0,
            ))
            .expect("destination");
        let packet = packet(&[
            0x90, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 10, 0xbe, 0xde, 0, 1, 0x31, 0xaa, 0xbb, 0,
        ]);
        let PacketView::Rtp(packet) = packet else {
            panic!("RTP packet");
        };
        let section = NegotiatedMediaSection::new(
            MediaSectionId::new(0),
            "0".to_owned(),
            crate::MediaKind::Audio,
            crate::MediaDirection::ReceiveOnly,
            Box::new([]),
            Box::new([crate::HeaderExtension::new(3, "urn:test".to_owned())]),
            None,
        );

        let forwarded = forwarder
            .forward_rtp(
                &packet,
                &section,
                StreamId::new(8),
                &[ExtensionRewrite::new(3, 5, Some(&[0xcc, 0xdd]))],
            )
            .expect("rewritten packet");

        assert_eq!(forwarded.bytes()[16], 0x51);
        assert_eq!(&forwarded.bytes()[17..19], &[0xcc, 0xdd]);
        let original = packet
            .header_extension(&section, 3)
            .expect("extension")
            .expect("present extension");
        assert_eq!(original.id(), 3);
        assert_eq!(original.value(), &[0xaa, 0xbb]);
    }
}
