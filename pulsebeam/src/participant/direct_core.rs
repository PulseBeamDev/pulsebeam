use std::{collections::VecDeque, time::Duration};

use ahash::{HashMap, HashMapExt};
use pulsebeam_rtc::{
    ChannelId, ConnectionId, DataChannelEvent, DataChannelOpen, MediaDirection, MediaEvent,
    MediaKind as RtcMediaKind, ReceiveStream, SendId, SendStream, StreamId, TransportEvent,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use tokio::time::Instant;

#[cfg(debug_assertions)]
use crate::rtp::egress_guard::EgressGuard;
use crate::{
    entity::{ParticipantId, RoomId, TrackKind},
    id::ShardId,
    keys::TrackKey,
    participant::{
        TrackPacket,
        batcher::{Batcher, NetworkEgress, OwnedPacketQueue},
        data::{DataOpenError, DataState},
        direct_transport::{DirectTransport, DirectTransportConfig, DirectTransportOutput},
        downstream::{DownstreamAllocator, SlotConfig},
        event::ParticipantSink,
        pacer::PacketPacer,
        reverse::ReversePacket,
        signaling::Signaling,
        upstream::{IncomingRtpRoute, UpstreamAllocator},
    },
    rtp::cache::TrackStreamCache,
    rtp::{
        ABS_CAPTURE_TIME_EXTENSION_URI, Codec, CodecPayloadTypes, KeyframeRequest,
        KeyframeRequestKind, MediaKind, MediaSectionId, PayloadType, Ssrc,
    },
    track::{
        self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent,
        DataTrackIntentError, StreamWrite, StreamWriter, Track, TrackMeta,
    },
};

use super::{ParticipantEffect, ParticipantInput, TrackPacketRef};

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MID_EXTENSION_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
const RID_EXTENSION_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
const TWCC_EXTENSION_URI: &str = "transport-wide-cc";
const RTP_HISTORY_CAPACITY: usize = 512;
const MAX_RETRANSMISSIONS_PER_PACKET: u8 = 2;
const RETRANSMISSION_INTERVAL: Duration = Duration::from_millis(50);
const MAX_RETRANSMISSIONS_PER_TICK: usize = 64;

#[derive(Clone)]
struct OutgoingExtensions {
    absolute_capture_time: Option<u8>,
    mid: Option<(u8, Box<[u8]>)>,
    rid: Option<u8>,
    twcc: Option<u8>,
    dependency_descriptor: Option<u8>,
    nackable: bool,
}

impl OutgoingExtensions {
    fn from_section(section: &pulsebeam_rtc::NegotiatedMediaSection) -> Self {
        let absolute_capture_time = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == ABS_CAPTURE_TIME_EXTENSION_URI)
            .map(pulsebeam_rtc::HeaderExtension::id)
            .filter(|&id| id > 0);
        let mid = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == MID_EXTENSION_URI)
            .and_then(|extension| {
                let id = extension.id();
                (id > 0 && section.mid().len() <= usize::from(u8::MAX))
                    .then(|| (id, section.mid().as_bytes().into()))
            });
        let twcc = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri().contains(TWCC_EXTENSION_URI))
            .map(pulsebeam_rtc::HeaderExtension::id)
            .filter(|&id| id > 0);
        let rid = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == RID_EXTENSION_URI)
            .map(pulsebeam_rtc::HeaderExtension::id)
            .filter(|&id| id > 0);
        let dependency_descriptor = section
            .header_extensions()
            .iter()
            .find(|extension| extension.uri() == pulsebeam_core::dd::URI)
            .map(pulsebeam_rtc::HeaderExtension::id)
            .filter(|&id| id > 0);
        let nackable = section.kind() == RtcMediaKind::Video
            && section.codecs().iter().any(pulsebeam_rtc::Codec::nack);
        Self {
            absolute_capture_time,
            mid,
            rid,
            twcc,
            dependency_descriptor,
            nackable,
        }
    }
}

struct SentRtp {
    sequence: u16,
    bytes: Vec<u8>,
    extended_sequence: u64,
    twcc_offset: Option<usize>,
    retransmissions: u8,
    last_retransmission: Option<Instant>,
}

struct RtpHistory {
    entries: Box<[Option<SentRtp>]>,
}

impl RtpHistory {
    fn new() -> Self {
        debug_assert!(RTP_HISTORY_CAPACITY.is_power_of_two());
        let entries = std::iter::repeat_with(|| None)
            .take(RTP_HISTORY_CAPACITY)
            .collect();
        Self { entries }
    }

    fn store(
        &mut self,
        sequence: u16,
        bytes: Vec<u8>,
        extended_sequence: u64,
        twcc_offset: Option<usize>,
    ) {
        let index = usize::from(sequence) & (RTP_HISTORY_CAPACITY - 1);
        let Some(entry) = self.entries.get_mut(index) else {
            debug_assert!(false, "RTP history index escapes its fixed ring");
            return;
        };
        *entry = Some(SentRtp {
            sequence,
            bytes,
            extended_sequence,
            twcc_offset,
            retransmissions: 0,
            last_retransmission: None,
        });
    }

    fn prepare_retransmission(
        &mut self,
        sequence: u16,
        now: Instant,
    ) -> Option<(Vec<u8>, u64, Option<usize>)> {
        let index = usize::from(sequence) & (RTP_HISTORY_CAPACITY - 1);
        let entry = self.entries.get_mut(index)?.as_mut()?;
        if entry.sequence != sequence
            || entry.retransmissions >= MAX_RETRANSMISSIONS_PER_PACKET
            || entry
                .last_retransmission
                .is_some_and(|last| now.saturating_duration_since(last) < RETRANSMISSION_INTERVAL)
        {
            return None;
        }
        entry.retransmissions = entry.retransmissions.saturating_add(1);
        entry.last_retransmission = Some(now);
        Some((
            entry.bytes.clone(),
            entry.extended_sequence,
            entry.twcc_offset,
        ))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    reason = "negotiated RTP fields and extension lengths are validated before encoding"
)]
fn encode_rtp(
    packet: &crate::rtp::RtpPacket,
    payload_type: PayloadType,
    ssrc: Ssrc,
    extensions: Option<&OutgoingExtensions>,
) -> (Vec<u8>, Option<usize>) {
    let absolute_capture_time = extensions
        .and_then(|extensions| extensions.absolute_capture_time)
        .zip(packet.extensions.absolute_capture_time.as_deref())
        .filter(|(_, value)| matches!(value.len(), 8 | 16));
    let mid = extensions.and_then(|extensions| extensions.mid.as_ref());
    let rid = extensions
        .and_then(|extensions| extensions.rid)
        .zip(packet.extensions.rid.as_ref())
        .map(|(id, rid)| (id, rid.as_bytes()))
        .filter(|(_, rid)| !rid.is_empty());
    let twcc = extensions.and_then(|extensions| extensions.twcc);
    let dependency_descriptor = extensions
        .and_then(|extensions| extensions.dependency_descriptor)
        .zip(packet.extensions.raw_dependency_descriptor.as_ref())
        .map(|(id, descriptor)| (id, descriptor.0.as_slice()))
        .filter(|(_, descriptor)| !descriptor.is_empty());
    let two_byte_extensions = absolute_capture_time
        .is_some_and(|(id, value)| id > 14 || value.len() > 16)
        || mid.is_some_and(|(id, _)| *id > 14)
        || rid.is_some_and(|(id, rid)| id > 14 || rid.len() > 16)
        || twcc.is_some_and(|id| id > 14)
        || dependency_descriptor.is_some_and(|(id, descriptor)| id > 14 || descriptor.len() > 16);
    let extension_header_len = if two_byte_extensions { 2usize } else { 1usize };
    let extension_data_len = absolute_capture_time
        .map_or(0, |(_, value)| {
            extension_header_len.saturating_add(value.len())
        })
        .saturating_add(mid.map_or(0, |(_, value)| {
            extension_header_len.saturating_add(value.len())
        }))
        .saturating_add(rid.map_or(0, |(_, value)| {
            extension_header_len.saturating_add(value.len())
        }))
        .saturating_add(twcc.map_or(0, |_| extension_header_len.saturating_add(2)))
        .saturating_add(dependency_descriptor.map_or(0, |(_, descriptor)| {
            extension_header_len.saturating_add(descriptor.len())
        }));
    let extension_bytes = if extension_data_len == 0 {
        0
    } else {
        4usize.saturating_add(extension_data_len.saturating_add(3) & !3)
    };
    let length = 12usize
        .saturating_add(extension_bytes)
        .saturating_add(packet.payload.len());
    let mut bytes = Vec::with_capacity(length);
    bytes.push(if extension_bytes == 0 { 0x80 } else { 0x90 });
    bytes.push((u8::from(packet.marker) << 7) | payload_type.get());
    bytes.extend_from_slice(&packet.seq_no.as_u16().to_be_bytes());
    bytes.extend_from_slice(&(packet.rtp_ts.numer() as u32).to_be_bytes());
    bytes.extend_from_slice(&ssrc.get().to_be_bytes());
    let mut twcc_offset = None;
    if extension_bytes != 0 {
        bytes.extend_from_slice(
            &(if two_byte_extensions {
                0x1000u16
            } else {
                0xbedeu16
            })
            .to_be_bytes(),
        );
        bytes.extend_from_slice(
            &u16::try_from(extension_bytes.saturating_sub(4).saturating_div(4))
                .expect("RTP header extension must fit its 16-bit word length")
                .to_be_bytes(),
        );
        if let Some((id, value)) = absolute_capture_time {
            debug_assert!(id > 0 && matches!(value.len(), 8 | 16));
            if two_byte_extensions {
                bytes.extend_from_slice(&[id, u8::try_from(value.len()).unwrap_or_default()]);
            } else {
                debug_assert!(id < 15 && value.len() <= 16);
                bytes.push((id << 4) | u8::try_from(value.len() - 1).unwrap_or_default());
            }
            bytes.extend_from_slice(value);
        }
        if let Some((id, mid)) = mid {
            debug_assert!(*id > 0);
            debug_assert!(!mid.is_empty() && mid.len() <= usize::from(u8::MAX));
            if two_byte_extensions {
                bytes.extend_from_slice(&[*id, u8::try_from(mid.len()).unwrap_or_default()]);
            } else {
                debug_assert!(*id < 15 && mid.len() <= 16);
                bytes.push((*id << 4) | u8::try_from(mid.len() - 1).unwrap_or_default());
            }
            bytes.extend_from_slice(mid);
        }
        if let Some((id, rid)) = rid {
            debug_assert!(id > 0 && !rid.is_empty() && rid.len() <= usize::from(u8::MAX));
            if two_byte_extensions {
                bytes.extend_from_slice(&[id, u8::try_from(rid.len()).unwrap_or_default()]);
            } else {
                debug_assert!(id < 15 && rid.len() <= 16);
                bytes.push((id << 4) | u8::try_from(rid.len() - 1).unwrap_or_default());
            }
            bytes.extend_from_slice(rid);
        }
        if let Some(id) = twcc {
            debug_assert!(id > 0);
            if two_byte_extensions {
                bytes.extend_from_slice(&[id, 2]);
            } else {
                debug_assert!(id < 15);
                bytes.push((id << 4) | 1);
            }
            twcc_offset = Some(bytes.len());
            bytes.extend_from_slice(&[0, 0]);
        }
        if let Some((id, descriptor)) = dependency_descriptor {
            debug_assert!(id > 0 && descriptor.len() <= usize::from(u8::MAX));
            if two_byte_extensions {
                bytes.extend_from_slice(&[id, u8::try_from(descriptor.len()).unwrap_or_default()]);
            } else {
                debug_assert!(id < 15 && descriptor.len() <= 16);
                bytes.push((id << 4) | u8::try_from(descriptor.len() - 1).unwrap_or_default());
            }
            bytes.extend_from_slice(descriptor);
        }
        bytes.resize(12usize.saturating_add(extension_bytes), 0);
    }
    bytes.extend_from_slice(&packet.payload);
    debug_assert_eq!(bytes.len(), length);
    (bytes, twcc_offset)
}

fn picture_loss_indication(media_ssrc: u32) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0] = 0x81;
    bytes[1] = 206;
    bytes[2..4].copy_from_slice(&2u16.to_be_bytes());
    bytes[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    bytes
}

fn full_intra_request(media_ssrc: u32, sequence: u8) -> [u8; 20] {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x84;
    bytes[1] = 206;
    bytes[2..4].copy_from_slice(&4u16.to_be_bytes());
    bytes[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    bytes[12..16].copy_from_slice(&media_ssrc.to_be_bytes());
    bytes[16] = sequence;
    bytes
}

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error: {0}")]
    Rtc(#[from] pulsebeam_rtc::LiveConnectionError),
    #[error("Signaling error: {0}")]
    Signaling(#[from] crate::participant::signaling::SignalingError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Invalid data channel intent: {0}")]
    InvalidDataTrackIntent(#[from] DataTrackIntentError),
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error("Exceeded maximum data topic channels")]
    TooManyDataTopicChannels,
}

pub struct DirectParticipantCore {
    transport: Box<DirectTransport>,
    sections: HashMap<pulsebeam_rtc::MediaSectionId, MediaSectionId>,
    receive_sections: HashMap<u8, pulsebeam_rtc::MediaSectionId>,
    send_sections: HashMap<u32, MediaSectionId>,
    send_extensions: HashMap<u32, OutgoingExtensions>,
    rtp_history: HashMap<u32, RtpHistory>,
    upstream: UpstreamAllocator,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    pacer: PacketPacer,
    pub(crate) participant_id: ParticipantId,
    pub(crate) room_id: RoomId,
    pub(crate) shard_id: ShardId,
    participant_key: crate::keys::ParticipantKey,
    next_send_id: u64,
    next_fir_sequence: u8,
    #[cfg(debug_assertions)]
    egress_guard: EgressGuard,
    ingress_shard: Option<ShardId>,
    udp_packets: OwnedPacketQueue,
    tcp_batcher: Batcher,
    pending_data: VecDeque<(ChannelId, Vec<u8>)>,
    data: DataState,
    signaling: Signaling,
    last_slow_poll: Instant,
    disconnect_reason: Option<DisconnectReason>,
}

impl DirectParticipantCore {
    #[allow(
        clippy::too_many_arguments,
        reason = "participant materialization takes its complete owned shard configuration"
    )]
    pub fn new(
        connection_id: ConnectionId,
        session: pulsebeam_rtc::NegotiatedSession,
        local: pulsebeam_rtc::LocalTransport,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        participant_key: crate::keys::ParticipantKey,
        manual_sub: bool,
        udp_gso_size: usize,
        tcp_gso_size: usize,
        now: Instant,
    ) -> Result<Self, pulsebeam_rtc::LiveConnectionError> {
        debug_assert!(
            std::mem::size_of::<Self>() < 4096,
            "participant core must keep live protocol components on the heap"
        );
        let mut sections = HashMap::with_capacity(session.media_sections().len());
        let mut receive_sections = HashMap::with_capacity(session.media_sections().len());
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
        downstream.update_allocation_input(
            now,
            crate::participant::allocation::AllocationInput {
                estimate: crate::participant::downstream::INITIAL_BANDWIDTH,
                application_limited: true,
            },
        );
        let mut send_streams = Vec::new();
        let mut send_sections = HashMap::with_capacity(session.media_sections().len());
        let mut send_extensions = HashMap::with_capacity(session.media_sections().len());
        for section in session.media_sections() {
            let mid = MediaSectionId::from(section.mid());
            sections.insert(section.id(), mid);
            if section.direction() == MediaDirection::ReceiveOnly {
                for codec in section.codecs() {
                    if Codec::from_name(codec.name()).is_some() {
                        receive_sections
                            .entry(codec.payload_type())
                            .or_insert_with(|| section.id());
                    }
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
                    TrackKind::Video => track::new_video(
                        mid,
                        meta,
                        section
                            .receive_rids()
                            .iter()
                            .map(|rid| crate::rtp::SimulcastEncoding::new(rid.as_str()))
                            .collect(),
                    ),
                    TrackKind::Data => continue,
                };
                let _ = upstream.add_published_track(mid, sender, track);
            }
            if section.direction() == MediaDirection::SendOnly {
                let mut payload_types = CodecPayloadTypes::default();
                for codec in section.codecs() {
                    let Some(codec_kind) = Codec::from_name(codec.name()) else {
                        continue;
                    };
                    let Some(payload_type) = PayloadType::new(codec.payload_type()) else {
                        debug_assert!(false, "negotiated RTP payload type must be valid");
                        continue;
                    };
                    payload_types.insert(codec_kind, payload_type);
                }
                if payload_types.is_empty() {
                    continue;
                }
                let kind = match section.kind() {
                    RtcMediaKind::Audio => MediaKind::Audio,
                    RtcMediaKind::Video => MediaKind::Video,
                    RtcMediaKind::Application => continue,
                };
                let ssrc = Ssrc::from(
                    u32::try_from(connection_id.get())
                        .unwrap_or(u32::MAX)
                        .wrapping_mul(0x9e37_79b9)
                        .wrapping_add(section.id().get() as u32)
                        .max(1),
                );
                downstream.add_slot(SlotConfig {
                    mid,
                    rid: None,
                    ssrc,
                    payload_types,
                    kind,
                });
                send_streams.push((section.id(), ssrc));
                send_sections.insert(ssrc.get(), mid);
                send_extensions.insert(ssrc.get(), OutgoingExtensions::from_section(section));
            }
        }
        let config = DirectTransportConfig::new(connection_id, session, local);
        let mut transport = Box::new(DirectTransport::new(config, now)?);
        for (section, ssrc) in send_streams {
            let id = StreamId::new(ssrc.get());
            if transport
                .register_send(SendStream::new(id, section, ssrc.get(), 0, 0))
                .is_err()
            {
                debug_assert!(false, "one direct send stream per negotiated media section");
            }
        }
        let mut core = Self {
            transport,
            sections,
            receive_sections,
            send_sections,
            send_extensions,
            rtp_history: HashMap::with_capacity(8),
            upstream,
            downstream,
            stream_writer: StreamWriter::new(),
            pacer: PacketPacer::new(now, crate::participant::downstream::INITIAL_BANDWIDTH.get()),
            participant_id,
            room_id,
            shard_id,
            participant_key,
            next_send_id: 0,
            next_fir_sequence: 0,
            #[cfg(debug_assertions)]
            egress_guard: EgressGuard::new(),
            ingress_shard: None,
            udp_packets: OwnedPacketQueue::with_capacity(udp_gso_size),
            tcp_batcher: Batcher::with_capacity(tcp_gso_size),
            pending_data: VecDeque::with_capacity(64),
            data: DataState::new(),
            signaling: Signaling::new(crate::log::LogCtx {
                room_id,
                participant_id,
            }),
            last_slow_poll: now,
            disconnect_reason: None,
        };
        core.signaling
            .set_slot_count(core.downstream.video.slot_count());
        core.signaling
            .set_audio_slot_count(core.downstream.audio_slot_count());
        Ok(core)
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

    pub(crate) fn report_departure(&mut self, send_id: pulsebeam_rtc::SendId, now: Instant) {
        if self.transport.report_departure(send_id, now).is_err() {
            debug_assert!(false, "every accepted media packet has one GCC send record");
        }
    }

    pub(crate) fn apply(&mut self, effect: ParticipantEffect) {
        match effect {
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                self.signaling.apply_participants(added, removed);
            }
            ParticipantEffect::TrackCandidateAdded { key, track } => {
                if self
                    .downstream
                    .add_track_candidate(key, &track, &self.data.channels_snapshot())
                    && track.kind() != TrackKind::Data
                {
                    self.downstream.install_track(key, track);
                    self.signaling.mark_tracks_dirty();
                    self.signaling.mark_assignments_dirty();
                }
            }
            ParticipantEffect::TrackCandidateRemoved { key, track_id } => {
                if self.downstream.remove_track_candidate(key).is_some()
                    && track_id.kind() != TrackKind::Data
                {
                    let _ = self.downstream.remove_track(&track_id);
                    self.signaling.mark_tracks_dirty();
                    self.signaling.mark_assignments_dirty();
                }
            }
            ParticipantEffect::TrackSubscribed { key, track_id } => {
                self.downstream.activate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackUnsubscribed { key, track_id } => {
                self.downstream.deactivate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackPublished { key, track_id } => {
                self.upstream.bind_track_key(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.bind_source(track_id, key);
                }
            }
            ParticipantEffect::TrackUnpublished { key, track_id } => {
                self.upstream.unbind_track_key(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.unpublish(track_id);
                }
            }
        }
    }

    pub(crate) fn input(&mut self, input: ParticipantInput<'_>) {
        match input {
            ParticipantInput::Network {
                batch,
                source_shard,
            } => self.enqueue_ingress(batch, source_shard),
            ParticipantInput::Timeout(now) => self.transport.handle_timeout(now),
            ParticipantInput::Track { key, packet, cache } => self.handle_track(key, packet, cache),
            ParticipantInput::Reverse { stream, packet } => self.handle_reverse(stream, packet),
        }
    }

    fn handle_track(
        &mut self,
        key: TrackKey,
        packet: TrackPacketRef<'_>,
        cache: Option<&TrackStreamCache>,
    ) {
        match packet {
            TrackPacketRef::Rtp(packet) => {
                let Some(entry) = self.downstream.track_candidate(key) else {
                    debug_assert!(false, "a TrackPacket must target an installed track");
                    return;
                };
                if entry.participant_id == self.participant_id {
                    debug_assert!(false, "a participant must never receive its own track");
                    return;
                }
                match entry.track_id.kind() {
                    TrackKind::Video => {
                        if self.downstream.on_forward_rtp(
                            key,
                            packet.arrival_ts,
                            cache,
                            &mut self.stream_writer,
                        ) {
                            self.signaling.mark_assignments_dirty();
                        }
                    }
                    TrackKind::Audio => {
                        let origin = crate::entity::AudioOrigin {
                            participant: entry.participant_id,
                            track: entry.track_id,
                        };
                        self.downstream.on_forward_audio_rtp(
                            origin,
                            packet,
                            &mut self.stream_writer,
                        );
                        if self.downstream.take_audio_speakers_changed() {
                            self.signaling.mark_assignments_dirty();
                        }
                    }
                    TrackKind::Data => debug_assert!(false, "data tracks do not carry RTP"),
                }
            }
            TrackPacketRef::Data { lane: _, bytes } => {
                let Some(channel) = self.downstream.data.forwarding(key) else {
                    return;
                };
                self.downstream.data.record_delivery(key, bytes.len());
                if self.pending_data.len() < self.pending_data.capacity() {
                    self.pending_data.push_back((channel, bytes.to_vec()));
                }
            }
        }
    }

    fn handle_reverse(&mut self, stream: TrackKey, packet: ReversePacket) {
        match packet.decode() {
            Some(crate::participant::reverse::ReverseInput::Keyframe { rid, kind }) => {
                self.request_remote_keyframe(stream, rid.as_ref(), kind);
            }
            Some(crate::participant::reverse::ReverseInput::ReliableControl(bytes)) => {
                let Some(channel) = self.upstream.data.source(stream) else {
                    return;
                };
                if self.pending_data.len() < self.pending_data.capacity() {
                    self.pending_data.push_back((channel, bytes));
                }
            }
            None => debug_assert!(false, "reverse route carried an invalid endpoint envelope"),
        }
    }

    fn request_remote_keyframe(
        &mut self,
        stream: TrackKey,
        rid: Option<&crate::rtp::EncodingId>,
        kind: KeyframeRequestKind,
    ) {
        let Some(track_id) = self.upstream.track_for_fanout(stream) else {
            return;
        };
        let Some(route) = self.upstream.route_for_track(track_id, rid) else {
            return;
        };
        let mut bytes = [0u8; 20];
        let length = match kind {
            KeyframeRequestKind::Pli => {
                bytes[..12].copy_from_slice(&picture_loss_indication(route.ssrc.get()));
                12
            }
            KeyframeRequestKind::Fir => {
                let sequence = self.next_fir_sequence;
                self.next_fir_sequence = self.next_fir_sequence.wrapping_add(1);
                bytes.copy_from_slice(&full_intra_request(route.ssrc.get(), sequence));
                20
            }
        };
        let Some(rtcp) = bytes.get(..length) else {
            debug_assert!(
                false,
                "a fixed RTCP feedback buffer bounds its encoded length"
            );
            return;
        };
        if self.transport.send_rtcp(rtcp).is_err() {
            debug_assert!(false, "an active publisher must have an SRTCP egress path");
        }
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
                DirectTransportOutput::Data(event) => self.handle_data_event(event, events),
                DirectTransportOutput::Rtcp { nacks } => self.retransmit(&nacks, now),
            }
        }
        while let Some(event) = self.transport.poll_media_event() {
            match event {
                MediaEvent::StreamDiscovered { ssrc, payload_type } => {
                    let _ = self.discover_receive_stream(ssrc, payload_type);
                }
                MediaEvent::KeyframeRequest { ssrc } => {
                    self.handle_downstream_keyframe_request(Ssrc::from(ssrc), events);
                }
                MediaEvent::SenderReport { .. } | MediaEvent::Feedback { .. } => {}
            }
        }
        let mut congestion = None;
        while let Some(outcome) = self.transport.poll_congestion() {
            let estimate = outcome.estimate();
            congestion = Some((estimate.bitrate_bps(), estimate.application_limited()));
            if let Some(probe) = outcome.probe() {
                let target = probe.target_bitrate_bps().max(estimate.bitrate_bps());
                congestion = Some((target, estimate.application_limited()));
            }
        }
        if let Some((estimate, application_limited)) = congestion {
            self.downstream.update_allocation_input(
                now,
                crate::participant::allocation::AllocationInput {
                    estimate: crate::participant::allocation::Bitrate::bps(estimate),
                    application_limited,
                },
            );
        }
        Ok(processed)
    }

    pub(crate) fn poll(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> Option<Instant> {
        if self.disconnect_reason.is_some() {
            return None;
        }
        if self.process(now, events).is_err() {
            self.disconnect_reason = Some(DisconnectReason::IceDisconnected);
            events.exit();
            return None;
        }
        if now.saturating_duration_since(self.last_slow_poll) >= SLOW_POLL_INTERVAL {
            self.upstream.poll_slow(now);
            let (assignments_changed, _) = self.downstream.poll_slow(now, events);
            if assignments_changed {
                self.signaling.mark_assignments_dirty();
            }
            self.last_slow_poll = now;
        }
        if self.signaling.needs_poll() {
            let mut snapshot = self.downstream.signaling_snapshot();
            snapshot.participants = self.signaling.participants_snapshot();
            if let Some(output) = self.signaling.poll(&snapshot) {
                if self
                    .transport
                    .send_data(output.cid, true, output.bytes, now)
                    .is_ok()
                {
                    self.signaling.commit_sent();
                } else {
                    self.signaling.retry_pending();
                }
            }
        }
        self.write_pending(now);
        self.drain_transport_egress();
        let slow = self
            .last_slow_poll
            .checked_add(SLOW_POLL_INTERVAL)
            .unwrap_or(self.last_slow_poll);
        let pacer = self
            .stream_writer
            .front_pacing_size()
            .map(|bytes| self.pacer.next_ready(now, bytes));
        self.transport
            .next_deadline(now)
            .map_or(Some(slow), |deadline| Some(deadline.min(slow)))
            .map(|deadline| pacer.map_or(deadline, |pacer| deadline.min(pacer)))
    }

    fn write_pending(&mut self, now: Instant) {
        while let Some((channel, bytes)) = self.pending_data.pop_front() {
            if self.transport.send_data(channel, true, bytes, now).is_err() {
                break;
            }
        }
        self.pacer
            .set_rate(now, self.transport.congestion_bitrate_bps(now));
        while let Some(estimated_size) = self.stream_writer.front_pacing_size() {
            if !self.pacer.permits(now, estimated_size) {
                break;
            }
            let Some(write) = self.stream_writer.pop() else {
                debug_assert!(false, "a paced stream write must remain queued");
                break;
            };
            let (packet, _mid, _rid, payload_type, ssrc, _kind) = match write {
                StreamWrite::Video {
                    pkt,
                    mid,
                    rid,
                    pt,
                    ssrc,
                } => (pkt, mid, rid, pt, ssrc, MediaKind::Video),
                StreamWrite::Audio { pkt, mid, pt, ssrc } => {
                    (pkt, mid, None, pt, ssrc, MediaKind::Audio)
                }
            };
            #[cfg(debug_assertions)]
            if let Some(violation) = self.egress_guard.check(
                _mid,
                _rid,
                u64::from(packet.seq_no),
                packet.rtp_ts.numer(),
                packet.marker,
                _kind,
            ) {
                tracing::error!(%_mid, ?_rid, %violation, "egress stream invariant violated");
                #[cfg(feature = "sim")]
                pulsebeam_runtime::fatal!("egress stream invariant violated: {violation}");
            }
            let extensions = self.send_extensions.get(&ssrc.get());
            let nackable = extensions.is_some_and(|extensions| extensions.nackable);
            let (mut bytes, twcc_offset) = encode_rtp(&packet, payload_type, ssrc, extensions);
            let send_id = self.next_send_id();
            let result = if let Some(twcc_offset) = twcc_offset {
                match self.transport.assign_congestion(send_id, bytes.len()) {
                    Ok(congestion) => {
                        let sequence = congestion.transport_sequence().to_be_bytes();
                        let Some(slot) = bytes.get_mut(twcc_offset..twcc_offset.saturating_add(2))
                        else {
                            debug_assert!(false, "TWCC extension must fit the encoded RTP packet");
                            break;
                        };
                        slot.copy_from_slice(&sequence);
                        self.transport.send_rtp_with_assigned_congestion(
                            &bytes,
                            u64::from(packet.seq_no),
                            send_id,
                        )
                    }
                    Err(error) => Err(error),
                }
            } else {
                self.transport
                    .send_rtp_untracked(&bytes, u64::from(packet.seq_no))
            };
            if result.is_err() {
                break;
            }
            if nackable {
                self.rtp_history
                    .entry(ssrc.get())
                    .or_insert_with(RtpHistory::new)
                    .store(
                        packet.seq_no.as_u16(),
                        bytes,
                        u64::from(packet.seq_no),
                        twcc_offset,
                    );
            }
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_forwarded_media_for(
                self.participant_id,
                u64::try_from(packet.payload.len()).unwrap_or(u64::MAX),
            );
        }
    }

    fn retransmit(&mut self, nacks: &[pulsebeam_rtc::RtcpNack], now: Instant) {
        let mut remaining = MAX_RETRANSMISSIONS_PER_TICK;
        for nack in nacks {
            for &sequence in nack.sequences() {
                if remaining == 0 {
                    return;
                }
                let Some((mut bytes, extended_sequence, twcc_offset)) = self
                    .rtp_history
                    .get_mut(&nack.media_ssrc())
                    .and_then(|history| history.prepare_retransmission(sequence, now))
                else {
                    continue;
                };
                let send_id = self.next_send_id();
                let result = if let Some(twcc_offset) = twcc_offset {
                    let Ok(congestion) = self.transport.assign_congestion(send_id, bytes.len())
                    else {
                        continue;
                    };
                    let Some(slot) = bytes.get_mut(twcc_offset..twcc_offset.saturating_add(2))
                    else {
                        debug_assert!(false, "TWCC extension must fit retransmitted RTP");
                        continue;
                    };
                    slot.copy_from_slice(&congestion.transport_sequence().to_be_bytes());
                    self.transport.send_rtp_with_assigned_congestion(
                        &bytes,
                        extended_sequence,
                        send_id,
                    )
                } else {
                    self.transport.send_rtp_untracked(&bytes, extended_sequence)
                };
                if result.is_err() {
                    return;
                }
                remaining = remaining.saturating_sub(1);
            }
        }
    }

    fn drain_transport_egress(&mut self) {
        while let Some(datagram) = self.transport.poll_egress() {
            let (bytes, transport, send_id) = datagram.into_parts();
            let destination = transport.destination();
            let receipt = send_id.map(|send_id| crate::participant::batcher::DepartureReceipt {
                participant: self.participant_key,
                send_id,
            });
            match transport.protocol() {
                pulsebeam_rtc::TransportProtocol::Udp => {
                    self.udp_packets
                        .push_back_with_receipt(destination, bytes, receipt);
                }
                pulsebeam_rtc::TransportProtocol::Tcp => {
                    self.tcp_batcher
                        .push_back_with_receipt(destination, &bytes, receipt);
                }
            }
        }
    }

    pub(crate) fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
        loop {
            match egress.append_tcp(&mut self.tcp_batcher) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
    }

    fn handle_data_event(&mut self, event: DataChannelEvent, events: &mut impl ParticipantSink) {
        match event {
            DataChannelEvent::Open(open) => self.open_data_channel(open, events),
            DataChannelEvent::Close(channel) => {
                let Some(topic) = self.data.close(channel) else {
                    return;
                };
                self.downstream.data.close(channel);
                self.upstream.data.close(channel);
                self.release_data_channel(topic, events);
            }
            DataChannelEvent::Message {
                id,
                binary,
                payload,
            } => self.handle_data_message(id, binary, payload, events),
            DataChannelEvent::AssociationClosed | DataChannelEvent::Error => {
                self.disconnect_reason = Some(DisconnectReason::IceDisconnected);
                events.exit();
            }
            DataChannelEvent::AssociationConnected => {}
        }
    }

    fn open_data_channel(&mut self, open: DataChannelOpen, events: &mut impl ParticipantSink) {
        let intent = match DataTrackIntent::from_channel(open.label(), open.reliability()) {
            Ok(intent) => intent,
            Err(error) => {
                self.disconnect_reason = Some(error.into());
                events.exit();
                return;
            }
        };
        match intent {
            DataTrackIntent::InternalSignaling => self.signaling.set_cid(open.id()),
            DataTrackIntent::UserTopic(topic) => {
                if let Some(previous) = self.data.close(open.id()) {
                    self.release_data_channel(previous, events);
                }
                if let Err(error) = self.data.open(open.id(), topic.clone()) {
                    self.disconnect_reason = Some(match error {
                        DataOpenError::DuplicateDataChannelLabel(value) => {
                            DisconnectReason::DuplicateDataChannelLabel(value)
                        }
                        DataOpenError::TooManyDataTopicChannels => {
                            DisconnectReason::TooManyDataTopicChannels
                        }
                    });
                    events.exit();
                    return;
                }
                match topic.direction {
                    DataTrackDirection::Publish => {
                        let label = crate::track::publication_label(topic.lane, &topic.topic);
                        let track = Track::data(
                            TrackMeta {
                                room_id: self.room_id,
                                shard_id: self.shard_id,
                                id: self.participant_id.derive_track_id(TrackKind::Data, &label),
                                origin: self.participant_id,
                            },
                            topic.topic,
                            topic.lane,
                            None,
                        );
                        self.upstream.data.publish(open.id(), &track);
                        events.publish_track(track);
                    }
                    DataTrackDirection::Subscribe => events.subscribe_tracks(
                        DataState::selector(&topic),
                        crate::track::SelectionPolicy::All,
                    ),
                }
            }
        }
    }

    fn handle_data_message(
        &mut self,
        channel: ChannelId,
        binary: bool,
        payload: Vec<u8>,
        events: &mut impl ParticipantSink,
    ) {
        if Some(channel) == self.signaling.cid {
            match self.signaling.handle_input(&payload) {
                Ok(input_events) => {
                    for event in input_events {
                        let crate::participant::signaling::SignalingInputEvent::UpstreamTrackState {
                            mid,
                            active,
                        } = event;
                        if active
                            && let Some((track, in_topology)) =
                                self.upstream.announce_state_mut(mid)
                            && !*in_topology
                        {
                            *in_topology = true;
                            events.publish_track(track.clone());
                        }
                    }
                    self.downstream
                        .apply_signaling_intents(self.signaling.reconcile());
                }
                Err(error) => {
                    self.disconnect_reason = Some(error.into());
                    events.exit();
                }
            }
            return;
        }
        let Some(topic) = self.data.channel(channel).cloned() else {
            return;
        };
        if !binary {
            return;
        }
        match (topic.lane, topic.direction) {
            (lane, DataTrackDirection::Publish) => events.publish_track_packet(
                self.upstream.data.published_stream(channel),
                TrackPacket::Data {
                    lane,
                    bytes: payload,
                },
            ),
            (DataLane::Reliable, DataTrackDirection::Subscribe) => {
                if let Some(stream) = self.downstream.data.subscribed_stream(channel) {
                    events.request_reverse(stream, ReversePacket::reliable_control(payload));
                }
            }
            (DataLane::Realtime, DataTrackDirection::Subscribe) => {}
        }
    }

    fn release_data_channel(&mut self, topic: DataTopicChannel, events: &mut impl ParticipantSink) {
        match topic.direction {
            DataTrackDirection::Publish => {
                let label = crate::track::publication_label(topic.lane, &topic.topic);
                events
                    .unpublish_track(self.participant_id.derive_track_id(TrackKind::Data, &label));
            }
            DataTrackDirection::Subscribe => events.unsubscribe_tracks(DataState::selector(&topic)),
        }
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
            rid: packet.extensions.rid,
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
        if route.fanout.is_none()
            && let Some((track, in_topology)) = self.upstream.announce_state_mut(route.mid)
            && !*in_topology
        {
            *in_topology = true;
            events.publish_track(track.clone());
        }
        if processed.request_keyframe {
            self.request_upstream_keyframe(route, events);
        }
        if let Some(packet) = processed.first {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
        for packet in processed.remaining {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
    }

    fn handle_downstream_keyframe_request(
        &mut self,
        ssrc: Ssrc,
        events: &mut impl ParticipantSink,
    ) {
        let Some(mid) = self.send_sections.get(&ssrc.get()).copied() else {
            return;
        };
        let request = KeyframeRequest {
            mid,
            rid: None,
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

    fn request_upstream_keyframe(
        &mut self,
        route: IncomingRtpRoute,
        events: &mut impl ParticipantSink,
    ) {
        if let Some(fanout) = route.fanout {
            events.request_reverse(
                fanout,
                ReversePacket::keyframe(route.rid, KeyframeRequestKind::Pli),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_rtp_carries_negotiated_mid_and_twcc_fields() {
        let extensions = OutgoingExtensions {
            absolute_capture_time: None,
            mid: Some((3, Box::from(*b"video"))),
            rid: None,
            twcc: Some(5),
            dependency_descriptor: None,
            nackable: true,
        };
        let packet = crate::rtp::RtpPacket {
            payload: vec![1, 2, 3],
            ..Default::default()
        };
        let (bytes, twcc_offset) = encode_rtp(
            &packet,
            PayloadType::new(96).expect("valid RTP payload type"),
            Ssrc::from(17),
            Some(&extensions),
        );

        assert_eq!(bytes[0], 0x90);
        assert_eq!(&bytes[12..16], &[0xbe, 0xde, 0, 3]);
        assert_eq!(bytes[16], 0x34);
        assert_eq!(&bytes[17..22], b"video");
        let offset = twcc_offset.expect("TWCC is negotiated");
        assert_eq!(bytes[offset - 1], 0x51);
        assert_eq!(&bytes[offset..offset + 2], &[0, 0]);
        assert_eq!(&bytes[bytes.len() - 3..], &[1, 2, 3]);
    }

    #[test]
    fn outgoing_rtp_carries_the_selected_simulcast_rid() {
        let extensions = OutgoingExtensions {
            absolute_capture_time: None,
            mid: None,
            rid: Some(4),
            twcc: None,
            dependency_descriptor: None,
            nackable: true,
        };
        let packet = crate::rtp::RtpPacket {
            extensions: crate::rtp::PacketExtensions {
                rid: Some(crate::rtp::EncodingId::from("f")),
                ..Default::default()
            },
            payload: vec![1, 2, 3],
            ..Default::default()
        };
        let (bytes, twcc_offset) = encode_rtp(
            &packet,
            PayloadType::new(96).expect("valid RTP payload type"),
            Ssrc::from(17),
            Some(&extensions),
        );

        assert_eq!(&bytes[12..16], &[0xbe, 0xde, 0, 1]);
        assert_eq!(&bytes[16..18], &[0x40, b'f']);
        assert!(twcc_offset.is_none());
    }

    #[test]
    fn outgoing_rtp_preserves_long_dependency_descriptor() {
        let extensions = OutgoingExtensions {
            absolute_capture_time: None,
            mid: None,
            rid: None,
            twcc: None,
            dependency_descriptor: Some(4),
            nackable: true,
        };
        let descriptor = (0u8..17).collect();
        let packet = crate::rtp::RtpPacket {
            extensions: crate::rtp::PacketExtensions {
                raw_dependency_descriptor: Some(pulsebeam_core::dd::RawDependencyDescriptor(
                    descriptor,
                )),
                ..Default::default()
            },
            payload: vec![1, 2, 3],
            ..Default::default()
        };
        let (bytes, twcc_offset) = encode_rtp(
            &packet,
            PayloadType::new(96).expect("valid RTP payload type"),
            Ssrc::from(17),
            Some(&extensions),
        );

        assert_eq!(&bytes[12..16], &[0x10, 0, 0, 5]);
        assert_eq!(&bytes[16..18], &[4, 17]);
        assert_eq!(&bytes[18..35], &(0u8..17).collect::<Vec<_>>());
        assert!(twcc_offset.is_none());
        assert_eq!(&bytes[bytes.len() - 3..], &[1, 2, 3]);
    }

    #[test]
    fn outgoing_rtp_preserves_absolute_capture_time() {
        let extensions = OutgoingExtensions {
            absolute_capture_time: Some(3),
            mid: None,
            rid: None,
            twcc: None,
            dependency_descriptor: None,
            nackable: false,
        };
        let packet = crate::rtp::RtpPacket {
            extensions: crate::rtp::PacketExtensions {
                absolute_capture_time: Some(Box::new([1, 2, 3, 4, 5, 6, 7, 8])),
                ..Default::default()
            },
            payload: vec![1, 2, 3],
            ..Default::default()
        };
        let (bytes, twcc_offset) = encode_rtp(
            &packet,
            PayloadType::new(96).expect("valid RTP payload type"),
            Ssrc::from(17),
            Some(&extensions),
        );

        assert_eq!(&bytes[12..16], &[0xbe, 0xde, 0, 3]);
        assert_eq!(&bytes[16..25], &[0x37, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(twcc_offset.is_none());
    }

    #[test]
    fn direct_participant_core_keeps_live_protocol_state_off_the_stack() {
        assert!(
            std::mem::size_of::<DirectParticipantCore>() < 4096,
            "participant core must keep live protocol components on the heap"
        );
    }
}
