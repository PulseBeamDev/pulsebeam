use ahash::{HashMap, HashMapExt};
use pulsebeam_rtc::{
    ConnectionId, MediaDirection, MediaEvent, MediaKind as RtcMediaKind, ReceiveStream, SendId,
    SendStream, StreamId, TransportEvent,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId, TrackKind},
    id::ShardId,
    participant::{
        TrackPacket,
        direct_transport::{DirectTransport, DirectTransportConfig, DirectTransportOutput},
        downstream::{DownstreamAllocator, SlotConfig},
        event::ParticipantSink,
        reverse::ReversePacket,
        upstream::{IncomingRtpRoute, UpstreamAllocator},
    },
    rtp::{KeyframeRequest, KeyframeRequestKind, MediaKind, MediaSectionId, PayloadType, Ssrc},
    track::{self, StreamWriter, Track, TrackMeta},
};

pub struct DirectParticipantCore {
    transport: DirectTransport,
    sections: HashMap<pulsebeam_rtc::MediaSectionId, MediaSectionId>,
    receive_sections: HashMap<u8, pulsebeam_rtc::MediaSectionId>,
    upstream: UpstreamAllocator,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    participant_id: ParticipantId,
    room_id: RoomId,
    shard_id: ShardId,
    next_send_id: u64,
    publications: Vec<Track>,
    ingress_shard: Option<ShardId>,
}

impl DirectParticipantCore {
    pub fn new(
        connection_id: ConnectionId,
        session: pulsebeam_rtc::NegotiatedSession,
        local: pulsebeam_rtc::LocalTransport,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        manual_sub: bool,
        now: Instant,
    ) -> Result<Self, pulsebeam_rtc::LiveConnectionError> {
        let mut sections = HashMap::with_capacity(session.media_sections().len());
        let mut receive_sections = HashMap::with_capacity(session.media_sections().len());
        let mut published = Vec::new();
        let mut upstream = UpstreamAllocator::new(crate::log::LogCtx {
            room_id,
            participant_id,
        });
        let mut downstream = DownstreamAllocator::new(
            crate::log::LogCtx {
                room_id,
                participant_id,
            },
            manual_sub,
        );
        let mut send_streams = Vec::new();
        for section in session.media_sections() {
            let mid = MediaSectionId::from(section.mid());
            sections.insert(section.id(), mid);
            if section.direction() == MediaDirection::ReceiveOnly {
                for codec in section.codecs() {
                    receive_sections
                        .entry(codec.payload_type())
                        .or_insert(section.id());
                }
                let kind = match section.kind() {
                    RtcMediaKind::Audio => TrackKind::Audio,
                    RtcMediaKind::Video => TrackKind::Video,
                    RtcMediaKind::Application => continue,
                };
                let meta = TrackMeta {
                    room_id,
                    shard_id,
                    id: participant_id.derive_track_id(kind, &mid),
                    origin: participant_id,
                };
                let (sender, track) = match kind {
                    TrackKind::Audio => track::new_audio(mid, meta),
                    TrackKind::Video => track::new_video(mid, meta, Vec::new()),
                    TrackKind::Data => continue,
                };
                if upstream.add_published_track(mid, sender, track.clone()) {
                    published.push(track);
                }
            }
            if section.direction() == MediaDirection::SendOnly {
                let Some(codec) = section.codecs().first() else {
                    continue;
                };
                let Some(pt) = PayloadType::new(codec.payload_type()) else {
                    debug_assert!(false, "negotiated RTP payload type must be valid");
                    continue;
                };
                let kind = match section.kind() {
                    RtcMediaKind::Audio => MediaKind::Audio,
                    RtcMediaKind::Video => MediaKind::Video,
                    RtcMediaKind::Application => continue,
                };
                let ssrc = Ssrc::from(
                    (connection_id.get() as u32)
                        .wrapping_mul(0x9e37_79b9)
                        .wrapping_add(section.id().get() as u32)
                        .max(1),
                );
                downstream.add_slot(SlotConfig {
                    mid,
                    rid: None,
                    ssrc,
                    pt,
                    kind,
                });
                send_streams.push((section.id(), ssrc));
            }
        }
        let config = DirectTransportConfig::new(connection_id, session, local);
        let mut transport = DirectTransport::new(config, now)?;
        for (section, ssrc) in send_streams {
            let id = StreamId::new(ssrc.get());
            transport
                .register_send(SendStream::new(id, section, ssrc.get(), 0, 0))
                .expect("one direct send stream per negotiated media section");
        }
        Ok(Self {
            transport,
            sections,
            receive_sections,
            upstream,
            downstream,
            stream_writer: StreamWriter::new(),
            participant_id,
            room_id,
            shard_id,
            next_send_id: 0,
            publications: published,
            ingress_shard: None,
        })
    }

    pub fn transport_mut(&mut self) -> &mut DirectTransport {
        &mut self.transport
    }

    pub fn upstream_mut(&mut self) -> &mut UpstreamAllocator {
        &mut self.upstream
    }

    pub fn downstream_mut(&mut self) -> &mut DownstreamAllocator {
        &mut self.downstream
    }

    pub fn stream_writer_mut(&mut self) -> &mut StreamWriter {
        &mut self.stream_writer
    }

    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    pub const fn room_id(&self) -> RoomId {
        self.room_id
    }

    pub const fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    pub fn take_publications(&mut self) -> Vec<Track> {
        std::mem::take(&mut self.publications)
    }

    pub fn enqueue_ingress(&mut self, batch: RecvPacketBatch, source_shard: ShardId) {
        self.ingress_shard = Some(source_shard);
        self.transport.enqueue(batch);
    }

    pub(crate) fn process(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> Result<usize, pulsebeam_rtc::LiveConnectionError> {
        let processed = self.transport.process_ingress(now)?;
        while let Some(output) = self.transport.poll_output() {
            match output {
                DirectTransportOutput::Rtp { stream, packet } => {
                    self.handle_rtp(stream, packet, events);
                }
                DirectTransportOutput::Transport(event) => {
                    self.handle_transport_event(event, events);
                }
                DirectTransportOutput::Data(_) | DirectTransportOutput::Rtcp(_) => {}
            }
        }
        while let Some(event) = self.transport.poll_media_event() {
            match event {
                MediaEvent::StreamDiscovered { ssrc, payload_type } => {
                    let _ = self.discover_receive_stream(ssrc, payload_type);
                }
                MediaEvent::KeyframeRequest { ssrc } => {
                    self.handle_keyframe_request(Ssrc::from(ssrc), events);
                }
                MediaEvent::SenderReport { .. } | MediaEvent::Feedback { .. } => {}
            }
        }
        Ok(processed)
    }

    fn handle_rtp(
        &mut self,
        stream: ReceiveStream,
        packet: crate::rtp::RtpPacket,
        events: &mut impl ParticipantSink,
    ) {
        let Some(mid) = self.sections.get(&stream.media_section()).copied() else {
            debug_assert!(
                false,
                "a registered receive stream must have a media section"
            );
            return;
        };
        let Some((slot, track_id)) = self.upstream.slot_for_mid(mid) else {
            return;
        };
        let route = IncomingRtpRoute {
            ssrc: Ssrc::from(stream.ssrc()),
            mid,
            rid: None,
            upstream_slot: slot,
            track_id,
            fanout: self.upstream.track_fanout(track_id),
        };
        self.upstream.cache_route(route);
        let processed = self.upstream.handle_incoming_rtp(
            route.upstream_slot,
            route.mid,
            route.rid.as_ref(),
            packet,
            None,
        );
        if !processed.valid_route {
            self.upstream.remove_route(route.ssrc);
            return;
        }
        if processed.request_keyframe {
            self.handle_keyframe_request(route.ssrc, events);
        }
        if let Some(packet) = processed.first {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
        for packet in processed.remaining {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
    }

    fn handle_keyframe_request(&mut self, ssrc: Ssrc, events: &mut impl ParticipantSink) {
        let Some(route) = self.upstream.route_for_ssrc(ssrc) else {
            return;
        };
        let request = KeyframeRequest {
            mid: route.mid,
            rid: route.rid,
            kind: KeyframeRequestKind::Pli,
        };
        let Some(layer) = self.downstream.handle_keyframe_request(request) else {
            return;
        };
        let stream = layer.stream_id();
        if let Some(fanout) = self.upstream.track_fanout(stream.0) {
            events.request_reverse(
                fanout,
                ReversePacket::keyframe(stream.1, KeyframeRequestKind::Pli),
            );
        }
    }

    fn handle_transport_event(&mut self, event: TransportEvent, events: &mut impl ParticipantSink) {
        match event {
            TransportEvent::IceConnected => {
                let Some(source_shard) = self.ingress_shard else {
                    return;
                };
                let Some((source, destination)) = self.transport.ingress_context() else {
                    return;
                };
                events.connected(source, destination, source_shard);
            }
            TransportEvent::IceDisconnected
            | TransportEvent::IceFailed
            | TransportEvent::DtlsClosed => {
                events.exit();
            }
            TransportEvent::IceChecking | TransportEvent::DtlsConnected => {}
        }
    }

    pub fn discover_receive_stream(&mut self, ssrc: u32, payload_type: u8) -> bool {
        let Some(section) = self.receive_sections.get(&payload_type).copied() else {
            return false;
        };
        let id = StreamId::new(ssrc);
        self.transport
            .register_receive(ReceiveStream::new(id, section, ssrc))
            .is_ok()
    }

    pub fn register_send_stream(
        &mut self,
        section: pulsebeam_rtc::MediaSectionId,
        ssrc: Ssrc,
    ) -> bool {
        let id = StreamId::new(ssrc.get());
        self.transport
            .register_send(SendStream::new(id, section, ssrc.get(), 0, 0))
            .is_ok()
    }

    pub fn next_send_id(&mut self) -> SendId {
        let id = SendId::new(self.next_send_id);
        self.next_send_id = self.next_send_id.wrapping_add(1);
        id
    }
}
