use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use ahash::{HashMap, HashMapExt};
#[cfg(test)]
use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptedConnection, AuthenticatedPath, Connection, DataMessage, Event, NetworkInput,
    NetworkProtocol, Output, RtpExtension, RtpWrite, SessionInfo, TransmitTarget,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use tokio::time::Instant;

use crate::bitrate::Bitrate;
use crate::entity::TrackId;
use crate::media::{
    ChannelId, KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Pt, Rid, SeqNo, Ssrc,
};
use crate::participant::batcher::{Batcher, NetworkEgress, OwnedPacketQueue};
use crate::rtp::{Codec as RtpCodec, Frequency, MediaTime, RtpExtensions, RtpPacket};

pub(crate) const MAX_PENDING_INGRESS: usize = 256;
pub(crate) const MAX_PENDING_FANOUT: usize = 256;
const MAX_ENCODINGS: usize = super::upstream::MAX_UPSTREAM_ENCODED_STREAMS;

fn extend_serial(previous: Option<u64>, value: u64, bits: u32) -> u64 {
    let modulus = 1u64.wrapping_shl(bits);
    let mask = modulus.wrapping_sub(1);
    let Some(previous) = previous else {
        return modulus.saturating_add(value & mask);
    };
    let delta = value.wrapping_sub(previous) & mask;
    if delta < modulus.wrapping_shr(1) {
        previous.saturating_add(delta)
    } else {
        previous.saturating_sub(modulus.wrapping_sub(delta))
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "copy into RTC-owned immutable storage; Bytes never cross a shard handoff"
)]
fn rtc_bytes(value: &[u8]) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(value)
}

fn tcp_frame_payload(frame: &[u8]) -> Option<&[u8]> {
    let length = u16::from_be_bytes(frame.get(..2)?.try_into().ok()?);
    let payload = frame.get(2..)?;
    (usize::from(length) == payload.len()).then_some(payload)
}

fn egress_extensions(
    values: RtpExtensions,
    playout_delay: Option<(MediaTime, MediaTime)>,
    writer: &mut pulsebeam_core::dd::DependencyDescriptorWriter,
    mappings: &[pulsebeam_rtc::RtpExtensionInfo],
) -> Result<Vec<RtpExtension>, pulsebeam_rtc::WriteError> {
    let mut extensions = values
        .opaque
        .into_iter()
        .filter(|extension| mappings.iter().any(|mapping| mapping.uri == extension.uri))
        .filter(|extension| {
            !matches!(
                extension.uri.as_ref(),
                "urn:ietf:params:rtp-hdrext:sdes:mid"
                    | "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
                    | "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
                    | "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                    | "urn:ietf:params:rtp-hdrext:transport-wide-cc-02"
                    | pulsebeam_core::vla::URI
                    | pulsebeam_core::dd::URI
                    | "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
                    | "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
            )
        })
        .collect::<Vec<_>>();
    if let Some(mapping) = mappings.iter().find(|mapping| {
        matches!(
            mapping.uri.as_ref(),
            pulsebeam_core::dd::URI
                | "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
        )
    }) {
        let mut descriptor = [0; pulsebeam_core::dd::MAX_DD_LEN];
        let rewritten = values
            .dependency_descriptor
            .as_deref()
            .and_then(|dd| writer.write(dd, &mut descriptor).ok());
        let bytes = rewritten.and_then(|len| descriptor.get(..len)).or_else(|| {
            values
                .raw_dependency_descriptor
                .as_ref()
                .map(|raw| raw.0.as_slice())
        });
        if let Some(bytes) = bytes {
            extensions.push(RtpExtension {
                uri: mapping.uri.clone(),
                value: rtc_bytes(bytes),
            });
        }
    }
    if let Some((min, max)) = playout_delay {
        let millis = |time: MediaTime| {
            time.numer()
                .saturating_mul(1000)
                .checked_div(u64::from(time.frequency().get()))
                .ok_or(pulsebeam_rtc::WriteError::InvalidRtp)
        };
        let delay = pulsebeam_rtc::PlayoutDelay::from_millis(millis(min)?, millis(max)?)
            .map_err(|_| pulsebeam_rtc::WriteError::InvalidRtp)?;
        extensions.push(RtpExtension {
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
            value: rtc_bytes(&delay.to_bytes()),
        });
    }
    Ok(extensions)
}

pub(crate) enum TransportPollOutput {
    Pending,
    Timeout(Instant),
    Transmit,
    Event(Box<Event>),
    Closed(pulsebeam_rtc::CloseReason),
}
pub(crate) enum IngressResult {
    Empty,
    Received,
}
pub(crate) enum TransportMutation {
    Data {
        channel: ChannelId,
        bytes: Vec<u8>,
        required: bool,
    },
    Keyframe {
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    },
}

pub(crate) struct RtpWriteCommand {
    pub(crate) pkt: RtpPacket,
    pub(crate) track_id: TrackId,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) ssrc: Ssrc,
    pub(crate) pt: Pt,
    pub(crate) kind: MediaKind,
    pub(crate) now: Instant,
    pub(crate) playout_delay: Option<(MediaTime, MediaTime)>,
}

pub(crate) struct RtpIngress {
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) packet: RtpPacket,
}

pub(crate) enum AppliedMutation {
    Applied,
    ControlNotWritten,
    DataNotWritten,
    KeyframeUnavailable { mid: Mid, rid: Option<Rid> },
    KeyframeRequested { mid: Mid, rid: Option<Rid> },
    RtpNotWritten,
    RtpWritten,
}

struct PendingIngress {
    batch: RecvPacketBatch,
    source_shard: crate::id::ShardId,
}
struct InputObservation {
    path: AuthenticatedPath,
    source_shard: crate::id::ShardId,
    tcp_flow: Option<pulsebeam_runtime::net::tcp::TcpFlowId>,
}
struct Encoding {
    mid: Mid,
    rid: Option<Rid>,
    sequence: Option<u64>,
    timestamp: Option<u64>,
}
struct Sender {
    ssrc: Ssrc,
    pt: Pt,
    sequence_origin: Option<u64>,
    highest_accepted: Option<u64>,
    highest_reported: Option<u64>,
    source: Option<(TrackId, Ssrc)>,
    descriptor_writer: pulsebeam_core::dd::DependencyDescriptorWriter,
    extensions: Vec<pulsebeam_rtc::RtpExtensionInfo>,
}

impl Sender {
    fn rtp_index(&self, sequence: u64) -> Option<u64> {
        sequence.checked_sub(
            self.sequence_origin
                .unwrap_or_else(|| sequence & !u64::from(u16::MAX)),
        )
    }

    fn accept_index(&mut self, sequence: u64) {
        // A receiver starts its SRTP rollover counter at zero. Anchor only on an
        // accepted original, never on a rejected write, discard, or source switch.
        self.sequence_origin
            .get_or_insert_with(|| sequence & !u64::from(u16::MAX));
        self.highest_accepted = Some(self.highest_accepted.unwrap_or(sequence).max(sequence));
    }

    fn reception_progress(&mut self, extended_highest: u32) -> Option<SeqNo> {
        let highest = self.highest_accepted?;
        let origin = self.sequence_origin?;
        let index = extend_serial(
            Some(highest.checked_sub(origin)?),
            u64::from(extended_highest),
            32,
        );
        let progress = index.checked_add(origin)?;
        if progress > highest
            || self
                .highest_reported
                .is_some_and(|previous| progress <= previous)
        {
            return None;
        }
        self.highest_reported = Some(progress);
        Some(progress.into())
    }
}

pub(crate) struct Transport {
    rtc: Connection,
    session: SessionInfo,
    senders: HashMap<Mid, Sender>,
    encodings: HashMap<Ssrc, Encoding>,
    pending_reports: indexmap::IndexMap<u32, pulsebeam_rtc::SenderReport>,
    input_observation: Option<InputObservation>,
    udp_packets: OwnedPacketQueue,
    tcp_batcher: Batcher,
    tcp_flows: HashMap<SocketAddr, pulsebeam_runtime::net::tcp::TcpFlowId>,
    pending_ingress: VecDeque<PendingIngress>,
    pending_timeout: Option<Instant>,
    pending_mutations: VecDeque<TransportMutation>,
    rtc_deadline: Option<Instant>,
    rtc_clock: Instant,
    rtc_needs_drain: bool,
    exited: bool,
    #[cfg(debug_assertions)]
    egress_guard: crate::rtp::egress_guard::EgressGuard,
    #[cfg(feature = "sim")]
    pub(crate) sim_span: tracing::Span,
}

impl Transport {
    pub(crate) fn new(
        accepted: AcceptedConnection,
        udp_gso_size: usize,
        tcp_gso_size: usize,
        now: Instant,
        #[cfg(feature = "sim")] sim_span: tracing::Span,
    ) -> Self {
        let mut senders = HashMap::new();
        for stats in accepted.connection.stats().senders {
            if let Some(info) = accepted
                .session
                .senders
                .iter()
                .find(|info| info.mid == stats.mid)
                && let Ok(mid) = Mid::new(&stats.mid)
            {
                senders.insert(
                    mid,
                    Sender {
                        ssrc: stats.ssrc.into(),
                        pt: info.codec.payload_type.into(),
                        sequence_origin: None,
                        highest_accepted: None,
                        highest_reported: None,
                        source: None,
                        descriptor_writer: Default::default(),
                        extensions: info.extensions.to_vec(),
                    },
                );
            }
        }
        Self {
            rtc: accepted.connection,
            session: accepted.session,
            senders,
            encodings: HashMap::new(),
            pending_reports: Default::default(),
            input_observation: None,
            udp_packets: OwnedPacketQueue::with_capacity(udp_gso_size),
            tcp_batcher: Batcher::with_capacity(tcp_gso_size),
            tcp_flows: HashMap::new(),
            pending_ingress: VecDeque::new(),
            pending_timeout: None,
            pending_mutations: VecDeque::new(),
            rtc_deadline: None,
            rtc_clock: now,
            rtc_needs_drain: true,
            exited: false,
            #[cfg(debug_assertions)]
            egress_guard: crate::rtp::egress_guard::EgressGuard::new(),
            #[cfg(feature = "sim")]
            sim_span,
        }
    }

    pub(crate) fn bind_tcp_flow(
        &mut self,
        peer: SocketAddr,
        flow: pulsebeam_runtime::net::tcp::TcpFlowId,
    ) {
        self.tcp_flows.insert(peer, flow);
    }

    pub(crate) fn retire_tcp_flow(
        &mut self,
        peer: SocketAddr,
        flow: pulsebeam_runtime::net::tcp::TcpFlowId,
    ) {
        if self.tcp_flows.get(&peer) == Some(&flow) {
            self.tcp_flows.remove(&peer);
        }
    }

    pub(crate) fn incoming_layers(&self, mid: Mid) -> Vec<crate::media::SimulcastLayer> {
        self.session
            .inbound_media
            .iter()
            .find(|info| info.mid.as_ref() == mid.as_str())
            .map(|info| {
                info.rids
                    .iter()
                    .filter_map(|rid| {
                        Rid::new(rid)
                            .ok()
                            .map(|rid| crate::media::SimulcastLayer { rid })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn enqueue_ingress(
        &mut self,
        batch: RecvPacketBatch,
        source_shard: crate::id::ShardId,
    ) {
        if self.pending_ingress.len() >= MAX_PENDING_INGRESS {
            let _ = self.pending_ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.pending_ingress.push_back(PendingIngress {
            batch,
            source_shard,
        });
    }
    pub(crate) fn enqueue_timeout(&mut self, now: Instant) {
        self.pending_timeout = Some(now);
    }
    pub(crate) fn enqueue_mutation(&mut self, mutation: TransportMutation) -> bool {
        if self.pending_mutations.len() >= MAX_PENDING_FANOUT {
            metrics::counter!("participant_fanout_shed").increment(1);
            return false;
        }
        self.pending_mutations.push_back(mutation);
        true
    }
    pub(crate) fn apply_next_mutation(&mut self, _now: Instant) -> Option<AppliedMutation> {
        Some(match self.pending_mutations.pop_front()? {
            TransportMutation::Data {
                channel,
                bytes,
                required,
            } => {
                // Best-effort payload must not build a stale FIFO behind a blocked
                // SCTP send window. Required control keeps the bounded native queue.
                let queued =
                    !required
                        && self.rtc.stats().data_channels.iter().any(|stats| {
                            stats.channel == channel && stats.queued_payload_bytes > 0
                        });
                if queued {
                    metrics::counter!("participant_fanout_shed").increment(1);
                    AppliedMutation::DataNotWritten
                } else if self.write_channel(channel, true, &bytes) {
                    AppliedMutation::Applied
                } else if required {
                    AppliedMutation::ControlNotWritten
                } else {
                    metrics::counter!("participant_fanout_shed").increment(1);
                    AppliedMutation::DataNotWritten
                }
            }
            TransportMutation::Keyframe { mid, rid, kind } => {
                if self.request_keyframe(KeyframeRequest { mid, rid, kind }) {
                    AppliedMutation::KeyframeRequested { mid, rid }
                } else {
                    AppliedMutation::KeyframeUnavailable { mid, rid }
                }
            }
        })
    }

    pub(crate) fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                super::batcher::AppendStatus::Drained => break,
                super::batcher::AppendStatus::Full if !egress.flush() => break,
                super::batcher::AppendStatus::Full => {}
            }
        }
        loop {
            match egress.append_tcp(&mut self.tcp_batcher) {
                super::batcher::AppendStatus::Drained => break,
                super::batcher::AppendStatus::Full if !egress.flush() => break,
                super::batcher::AppendStatus::Full => {}
            }
        }
    }
    pub(crate) fn poll_output(&mut self) -> TransportPollOutput {
        if self.udp_packets.is_full() || self.tcp_batcher.is_full() {
            return TransportPollOutput::Pending;
        }
        match self.rtc.poll(self.rtc_clock.into()) {
            Output::Idle { next_wakeup } => {
                if next_wakeup.is_some_and(|deadline| deadline <= self.rtc_clock.into_std()) {
                    return TransportPollOutput::Pending;
                }
                // Only quiescence ends the processed input's attribution window.
                self.input_observation = None;
                TransportPollOutput::Timeout(next_wakeup.map(Into::into).unwrap_or_else(|| {
                    self.rtc_clock
                        .checked_add(Duration::from_secs(1))
                        .unwrap_or(self.rtc_clock)
                }))
            }
            Output::Transmit(tx) => {
                match tx.target {
                    TransmitTarget::Udp { remote, .. } => {
                        if !self.udp_packets.push_back(remote, tx.payload.to_vec()) {
                            metrics::counter!("participant_udp_egress_dropped_bytes", "reason" => "queue_full")
                                .increment(tx.payload.len() as u64);
                        }
                    }
                    TransmitTarget::IceTcp { local, remote, .. } => {
                        // The runtime adds RFC 4571 framing to queued datagrams.
                        match tcp_frame_payload(&tx.payload) {
                            Some([]) => {}
                            Some(payload) => {
                                // Deferred input's replies must stay on its socket, even after EOF.
                                let flow = self
                                    .input_observation
                                    .as_ref()
                                    .filter(|observation| {
                                        observation.path.protocol == NetworkProtocol::IceTcp
                                            && observation.path.local == local
                                            && observation.path.remote == remote
                                    })
                                    .map_or_else(
                                        || self.tcp_flows.get(&remote).copied(),
                                        |observation| observation.tcp_flow,
                                    );
                                if let Some(flow) = flow {
                                    if !self.tcp_batcher.push_on_flow(remote, flow, payload) {
                                        metrics::counter!("participant_tcp_egress_dropped_bytes", "reason" => "queue_full")
                                            .increment(payload.len() as u64);
                                    }
                                } else {
                                    metrics::counter!("participant_tcp_egress_dropped_bytes", "reason" => "missing_flow")
                                        .increment(tx.payload.len() as u64);
                                }
                            }
                            None => self.rtc.abort(),
                        }
                    }
                }
                TransportPollOutput::Transmit
            }
            Output::Event(event) => TransportPollOutput::Event(Box::new(event)),
            Output::Closed(reason) => TransportPollOutput::Closed(reason),
            _ => TransportPollOutput::Pending,
        }
    }
    pub(crate) fn write_channel(&mut self, cid: ChannelId, binary: bool, bytes: &[u8]) -> bool {
        let message = if binary {
            DataMessage::Binary(rtc_bytes(bytes))
        } else {
            DataMessage::Text(rtc_bytes(bytes))
        };
        let accepted = self
            .rtc
            .send_data(self.rtc_clock.into(), cid, message)
            .is_ok();
        self.rtc_needs_drain |= accepted;
        accepted
    }
    pub(crate) fn preferred_send_pt(&self, mid: Mid, _kind: MediaKind) -> Option<Pt> {
        self.senders.get(&mid).map(|sender| sender.pt)
    }
    pub(crate) fn stream_tx_ssrc(&self, mid: Mid) -> Option<Ssrc> {
        self.senders.get(&mid).map(|sender| sender.ssrc)
    }
    pub(crate) fn set_desired_bitrate(&mut self, bitrate: Bitrate) {
        self.rtc
            .set_desired_bitrate(Some(pulsebeam_rtc::MediaPayloadBitrate::from_bps(
                bitrate.as_bps(),
            )));
        self.rtc_needs_drain = true;
    }
    pub(crate) fn authenticated_source(
        &mut self,
        path: AuthenticatedPath,
    ) -> Option<crate::id::ShardId> {
        self.input_observation
            .as_ref()
            .filter(|observation| observation.path == path)
            .map(|observation| observation.source_shard)
    }
    pub(crate) fn register_encoding(
        &mut self,
        info: pulsebeam_rtc::EncodingInfo,
    ) -> Option<pulsebeam_rtc::SenderReport> {
        let mid = Mid::new(&info.mid).ok()?;
        let rid = info.rid.as_deref().map(Rid::new).transpose().ok()?;
        if self.encodings.len() >= MAX_ENCODINGS && !self.encodings.contains_key(&info.ssrc.into())
        {
            return None;
        }
        self.encodings.entry(info.ssrc.into()).or_insert(Encoding {
            mid,
            rid,
            sequence: None,
            timestamp: None,
        });
        self.pending_reports.shift_remove(&info.ssrc)
    }
    pub(crate) fn retire_encoding(&mut self, ssrc: u32) {
        self.encodings.remove(&ssrc.into());
        self.pending_reports.shift_remove(&ssrc);
    }
    pub(crate) fn convert_rtp(
        &mut self,
        rtp: pulsebeam_rtc::RtpPacket,
        mid: Mid,
        rid: Option<Rid>,
    ) -> Option<RtpIngress> {
        let media = self
            .session
            .inbound_media
            .iter()
            .find(|media| media.mid.as_ref() == mid.as_str())?;
        let codec = media
            .codecs
            .iter()
            .find(|codec| codec.payload_type == rtp.header.payload_type)?;
        let frequency = Frequency::from_nonzero(std::num::NonZeroU32::new(codec.clock_rate)?);
        let kind = match codec.profile {
            pulsebeam_rtc::CodecProfile::Opus => RtpCodec::Opus,
            pulsebeam_rtc::CodecProfile::H264 { .. } => RtpCodec::H264,
        };
        let encoding = self.encodings.get_mut(&rtp.header.ssrc.into())?;
        if encoding.mid != mid || encoding.rid != rid {
            return None;
        }
        let sequence = extend_serial(encoding.sequence, u64::from(rtp.header.sequence), 16);
        let timestamp = extend_serial(encoding.timestamp, u64::from(rtp.header.timestamp), 32);
        encoding.sequence = Some(encoding.sequence.unwrap_or(sequence).max(sequence));
        encoding.timestamp = Some(encoding.timestamp.unwrap_or(timestamp).max(timestamp));
        Some(RtpIngress {
            mid,
            rid,
            packet: RtpPacket::from_rtc(rtp, kind, frequency, sequence, timestamp, rid)?,
        })
    }
    pub(crate) fn sender_report(
        &mut self,
        report: pulsebeam_rtc::SenderReport,
    ) -> Option<(Mid, crate::rtp::sync::SenderInfo)> {
        let Some(encoding) = self.encodings.get_mut(&report.ssrc.into()) else {
            if self.pending_reports.len() >= MAX_ENCODINGS
                && !self.pending_reports.contains_key(&report.ssrc)
            {
                self.pending_reports.shift_remove_index(0);
            }
            self.pending_reports.insert(report.ssrc, report);
            return None;
        };
        let media = self
            .session
            .inbound_media
            .iter()
            .find(|media| media.mid.as_ref() == encoding.mid.as_str())?;
        let frequency =
            Frequency::from_nonzero(std::num::NonZeroU32::new(media.codecs.first()?.clock_rate)?);
        let timestamp = extend_serial(encoding.timestamp, u64::from(report.rtp_timestamp), 32);
        encoding.timestamp = Some(encoding.timestamp.unwrap_or(timestamp).max(timestamp));
        Some((
            encoding.mid,
            crate::rtp::sync::SenderInfo {
                ssrc: report.ssrc.into(),
                rtp_time: MediaTime::new(timestamp, frequency),
                ntp_time: crate::clock::NtpTime::from_raw(
                    u64::from(report.ntp_seconds).wrapping_shl(32) | u64::from(report.ntp_fraction),
                ),
            },
        ))
    }
    pub(crate) fn receiver_report(
        &mut self,
        report: pulsebeam_rtc::ReceiverReport,
    ) -> Option<(Mid, SeqNo)> {
        let mid = Mid::new(&report.mid).ok()?;
        let sender = self.senders.get_mut(&mid)?;
        if *sender.ssrc != report.ssrc {
            return None;
        }
        Some((
            mid,
            sender.reception_progress(report.extended_highest_sequence)?,
        ))
    }
    pub(crate) fn keyframe_request(&self, ssrc: u32) -> Option<KeyframeRequest> {
        let (&mid, _) = self
            .senders
            .iter()
            .find(|(_, sender)| *sender.ssrc == ssrc)?;
        Some(KeyframeRequest {
            mid,
            rid: None,
            kind: KeyframeRequestKind::Pli,
        })
    }
    pub(crate) fn has_pending_ingress(&self) -> bool {
        !self.pending_ingress.is_empty()
    }
    pub(crate) fn clock(&self) -> Instant {
        self.rtc_clock
    }
    pub(crate) fn advance_clock(&mut self, candidate: Instant, wall_now: Instant) {
        let previous = self.rtc_clock;
        self.rtc_clock = self.rtc_clock.max(candidate).max(wall_now);
        debug_assert!(
            self.rtc_clock >= previous,
            "transport RTC clock moved backwards"
        );
        debug_assert!(
            self.rtc_clock
                <= wall_now
                    .checked_add(pulsebeam_runtime::SHARD_TIMER_QUANTUM)
                    .unwrap_or(wall_now),
            "transport RTC clock advanced beyond one wheel quantum"
        );
    }
    pub(crate) fn needs_drain(&self) -> bool {
        self.rtc_needs_drain
    }
    pub(crate) fn mark_needs_drain(&mut self) {
        self.rtc_needs_drain = true;
    }
    pub(crate) fn set_drain_result(&mut self, deadline: Option<Instant>) -> bool {
        self.rtc_deadline = deadline;
        self.rtc_needs_drain = false;
        if deadline.is_none() {
            self.exited = true;
        }
        deadline.is_some()
    }
    pub(crate) fn take_timeout(&mut self) -> Option<Instant> {
        self.pending_timeout.take()
    }
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.rtc_deadline
    }
    pub(crate) fn is_exited(&self) -> bool {
        self.exited
    }
    pub(crate) fn request_keyframe(&mut self, request: KeyframeRequest) -> bool {
        let known = self
            .encodings
            .iter()
            .find(|(_, encoding)| encoding.mid == request.mid && encoding.rid == request.rid)
            .map(|(ssrc, _)| *ssrc);
        let mut accepted = false;
        if let Some(ssrc) = known {
            accepted = self.rtc.request_keyframe(*ssrc).is_ok();
        } else if let Some(media) = self
            .session
            .inbound_media
            .iter()
            .find(|media| media.mid.as_ref() == request.mid.as_str())
        {
            // A fresh publisher may need a keyframe before emitting its first
            // RTP. SDP does not bind these SSRCs to RIDs, so do not invent one.
            for &ssrc in media.primary_ssrcs.iter() {
                accepted |= self.rtc.request_keyframe(ssrc).is_ok();
            }
        }
        self.rtc_needs_drain |= accepted;
        accepted
    }
    pub(crate) fn apply_rtp_command(&mut self, command: RtpWriteCommand) -> AppliedMutation {
        let Some(sender) = self.senders.get_mut(&command.mid) else {
            return AppliedMutation::RtpNotWritten;
        };
        if command.rid.is_some() || sender.ssrc != command.ssrc || sender.pt != command.pt {
            return AppliedMutation::RtpNotWritten;
        }
        let source = (command.track_id, command.pkt.ssrc);
        if sender.source != Some(source) {
            if self.rtc.discard_pending_rtp(command.mid.as_str()).is_err() {
                return AppliedMutation::RtpNotWritten;
            }
            sender.descriptor_writer.reset();
            sender.source = Some(source);
        }
        let sequence = *command.pkt.seq_no;
        let timestamp = command.pkt.rtp_ts.numer();
        let marker = command.pkt.marker;
        let Ok(extensions) = egress_extensions(
            command.pkt.ext_vals,
            command.playout_delay,
            &mut sender.descriptor_writer,
            &sender.extensions,
        ) else {
            return AppliedMutation::RtpNotWritten;
        };
        let Some(index) = sender.rtp_index(sequence) else {
            return AppliedMutation::RtpNotWritten;
        };
        let packet = RtpWrite {
            sequence: index,
            timestamp: u32::try_from(timestamp & u64::from(u32::MAX)).unwrap_or_default(),
            wallclock: command.now.into(),
            marker,
            payload: rtc_bytes(&command.pkt.payload),
            extensions,
        };
        if let Err(rejected) =
            self.rtc
                .write_rtp(self.rtc_clock.into(), command.mid.as_str(), packet)
        {
            metrics::counter!("participant_rtp_write_rejected").increment(1);
            tracing::debug!(reason = ?rejected.reason, mid = %command.mid, "RTC rejected primary RTP");
            return AppliedMutation::RtpNotWritten;
        }
        if command.kind == MediaKind::Video {
            tracing::trace!(target: crate::log::TARGET_VIDEO, mid = %command.mid, sequence, timestamp, marker, "Writing RTP packet");
        }
        sender.accept_index(sequence);
        self.rtc_needs_drain = true;
        #[cfg(debug_assertions)]
        if let Some(violation) = self.egress_guard.check(
            command.mid,
            command.rid,
            sequence,
            timestamp,
            marker,
            command.kind,
        ) {
            tracing::error!(%violation, "egress stream invariant violated");
            #[cfg(feature = "sim")]
            pulsebeam_runtime::fatal!("egress stream invariant violated: {violation}");
        }
        AppliedMutation::RtpWritten
    }
    pub(crate) fn receive_pending(&mut self, at: Instant) -> IngressResult {
        let Some(pending) = self.pending_ingress.front_mut() else {
            return IngressResult::Empty;
        };
        let source_shard = pending.source_shard;
        let batch = &mut pending.batch;
        let protocol = match batch.transport {
            pulsebeam_runtime::net::Transport::Udp(_) => NetworkProtocol::Udp,
            pulsebeam_runtime::net::Transport::Tcp => NetworkProtocol::IceTcp,
        };
        let (remote, local, arrival, tcp_flow) = (
            batch.src,
            batch.dst,
            batch.received_at.monotonic,
            batch.tcp_flow,
        );
        let Some(packet) = batch.next_packet() else {
            self.pending_ingress.pop_front();
            return IngressResult::Empty;
        };
        let input = match protocol {
            NetworkProtocol::Udp => NetworkInput::Udp {
                local,
                remote,
                ecn: None,
                payload: rtc_bytes(packet),
            },
            NetworkProtocol::IceTcp => {
                let Ok(length) = u16::try_from(packet.len()) else {
                    metrics::counter!("participant_ingress_rtc_rejected").increment(1);
                    return IngressResult::Received;
                };
                let mut frame = Vec::with_capacity(packet.len().saturating_add(2));
                frame.extend_from_slice(&length.to_be_bytes());
                frame.extend_from_slice(packet);
                NetworkInput::IceTcp {
                    local,
                    remote,
                    frame: rtc_bytes(&frame),
                }
            }
        };
        self.input_observation = Some(InputObservation {
            path: AuthenticatedPath {
                protocol,
                local,
                remote,
                arrival: arrival.into(),
            },
            source_shard,
            tcp_flow,
        });
        if let Err(error) = self.rtc.receive(at.into(), arrival.into(), input) {
            metrics::counter!("participant_ingress_rtc_rejected").increment(1);
            tracing::debug!(?error, %remote, "RTC rejected ingress");
        }
        self.rtc_needs_drain = true;
        IngressResult::Received
    }
    pub(crate) fn timeout(&mut self) {
        self.rtc_needs_drain = true;
    }
    pub(crate) fn disconnect(&mut self) {
        self.pending_ingress.clear();
        self.pending_mutations.clear();
        self.pending_timeout = None;
        self.input_observation = None;
        self.udp_packets.discard();
        self.tcp_batcher.discard();
        self.rtc.abort();
        self.rtc_needs_drain = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_tcp_ingress_never_moves_its_output_to_a_replacement_flow() {
        use pulsebeam_core::net::{TcpListener, TcpStream};
        use pulsebeam_runtime::net::tcp::{BufferedTcpStream, TcpTransport};
        use tokio::io::AsyncReadExt;

        let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.16");
        pulsebeam_runtime::testing::run_local(host, async move {
            let (accepted, _, binding) =
                crate::control::application_only_test_connection_and_binding();
            let local = "127.0.0.1:6000".parse().unwrap();
            let remote = "127.0.0.1:5000".parse().unwrap();
            let mut transport = Transport::new(
                accepted,
                8,
                8,
                Instant::now(),
                #[cfg(feature = "sim")]
                tracing::Span::none(),
            );
            let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let (client, stream) = tokio::join!(TcpStream::connect(address), listener.accept());
            let original_client = client.unwrap();
            let (stream, _) = stream.unwrap();
            let mut framed = Vec::new();
            for _ in 0..2 {
                framed.extend_from_slice(&u16::try_from(binding.len()).unwrap().to_be_bytes());
                framed.extend_from_slice(&binding);
            }
            let mut socket = TcpTransport::new(local);
            let original = socket
                .add_connection(
                    BufferedTcpStream::with_buffered(
                        stream,
                        framed,
                        pulsebeam_runtime::net::ReceiveTime::now(),
                    ),
                    remote,
                )
                .unwrap();
            transport.bind_tcp_flow(remote, original);
            let mut frames = Vec::new();
            let mut closed = Vec::new();
            socket.try_recv_events(&mut frames, &mut closed).unwrap();
            assert_eq!(frames.len(), 2);
            assert!(closed.is_empty());
            for frame in frames.drain(..) {
                transport.enqueue_ingress(frame, crate::id::ShardId::new(0));
            }
            let receive_and_drain = |transport: &mut Transport| {
                let now = Instant::now();
                transport.advance_clock(now, now);
                loop {
                    match transport.receive_pending(now) {
                        IngressResult::Received => break,
                        IngressResult::Empty => assert!(transport.has_pending_ingress()),
                    }
                }
                loop {
                    match transport.poll_output() {
                        TransportPollOutput::Timeout(_) => break,
                        TransportPollOutput::Event(event) => {
                            if let Event::AuthenticatedPath(path) = *event {
                                assert_eq!(
                                    transport.authenticated_source(path),
                                    Some(crate::id::ShardId::new(0))
                                );
                            }
                        }
                        TransportPollOutput::Pending | TransportPollOutput::Transmit => {}
                        TransportPollOutput::Closed(reason) => {
                            panic!("unexpected close: {reason:?}")
                        }
                    }
                }
            };
            receive_and_drain(&mut transport);
            assert!(!transport.tcp_batcher.is_empty());
            drop(original_client);
            while closed.is_empty() {
                socket.readable().await.unwrap();
                let _ = socket.try_recv_events(&mut frames, &mut closed);
                assert!(frames.is_empty());
            }
            assert_eq!(closed, vec![(remote, original)]);
            transport.retire_tcp_flow(remote, original);
            let (client, stream) = tokio::join!(TcpStream::connect(address), listener.accept());
            let mut client = client.unwrap();
            let (stream, _) = stream.unwrap();
            let replacement = socket
                .add_connection(BufferedTcpStream::new(stream), remote)
                .unwrap();
            transport.bind_tcp_flow(remote, replacement);
            receive_and_drain(&mut transport);
            transport
                .tcp_batcher
                .push_on_flow(remote, replacement, b"new");
            assert!(transport.tcp_batcher.flush(&mut socket));
            assert!(transport.tcp_batcher.is_empty());
            let mut received = [0; 5];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"\0\x03new");
            for _ in 0..pulsebeam_runtime::net::BATCH_SIZE {
                assert!(transport.udp_packets.push_back(remote, vec![1]));
            }
            assert!(!transport.udp_packets.push_back(remote, vec![2]));
            assert!(matches!(
                transport.poll_output(),
                TransportPollOutput::Pending
            ));
            transport.disconnect();
            assert!(matches!(
                transport.poll_output(),
                TransportPollOutput::Closed(_)
            ));
            assert!(transport.pending_ingress.is_empty());
        });
    }

    fn extensions(
        values: RtpExtensions,
        delay: Option<(MediaTime, MediaTime)>,
    ) -> Vec<RtpExtension> {
        let mappings = [pulsebeam_rtc::RtpExtensionInfo {
            id: 1,
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
        }];
        egress_extensions(values, delay, &mut Default::default(), &mappings).unwrap()
    }

    #[test]
    fn fresh_default_egress_does_not_inherit_ingress_delay() {
        let ingress = RtpExtensions {
            opaque: vec![RtpExtension {
                uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
                value: Bytes::from_static(&[0, 0, 20]),
            }],
            ..Default::default()
        };
        assert!(extensions(ingress.clone(), None).is_empty());
        let fixed = extensions(
            ingress,
            Some((MediaTime::from_millis(50), MediaTime::from_millis(80))),
        );
        assert_eq!(fixed.len(), 1);
        assert_eq!(fixed[0].value.as_ref(), &[0, 0x50, 8]);
    }

    #[test]
    fn ingress_stream_identity_is_not_reused_on_egress() {
        let ingress = RtpExtensions {
            rid: Some(Rid::from("h")),
            opaque: vec![RtpExtension {
                uri: "urn:ietf:params:rtp-hdrext:sdes:mid".into(),
                value: Bytes::from_static(b"source"),
            }],
            ..Default::default()
        };
        assert!(extensions(ingress, None).is_empty());
    }

    #[test]
    fn first_accepted_original_anchors_receiver_roc_without_resetting_late_holes() {
        let mut sender = Sender {
            ssrc: 1.into(),
            pt: 111.into(),
            sequence_origin: None,
            highest_accepted: None,
            highest_reported: None,
            source: None,
            descriptor_writer: Default::default(),
            extensions: [].into(),
        };
        assert_eq!(sender.rtp_index(65535), Some(65535));
        assert_eq!(sender.rtp_index(65536), Some(0));
        sender.accept_index(65536);
        assert_eq!(sender.rtp_index(65539), Some(3));
        sender.accept_index(65539);
        assert_eq!(sender.rtp_index(65537), Some(1));
        assert_eq!(sender.reception_progress(3), Some(SeqNo::from(65539)));
        assert_eq!(sender.reception_progress(2), None);
        assert_eq!(sender.reception_progress(4), None);
    }

    #[test]
    fn serial_wrap_and_late_holes_preserve_the_high_water_era() {
        let before = extend_serial(None, 65534, 16);
        let after = extend_serial(Some(before), 1, 16);
        assert_eq!(after.saturating_sub(before), 3);
        assert_eq!(
            extend_serial(Some(after), 65535, 16),
            before.saturating_add(1)
        );
        let timestamp = extend_serial(None, u64::from(u32::MAX), 32);
        assert_eq!(
            extend_serial(Some(timestamp), 0, 32),
            timestamp.saturating_add(1)
        );
    }
}
