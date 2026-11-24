use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use ahash::{HashMap, HashMapExt};
#[cfg(test)]
use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptedConnection, AuthenticatedPath, DataMessage, Event, NetworkInput, NetworkProtocol,
    Output, RtcConnection, RtpExtension, RtpWrite, SessionInfo, TransmitTarget,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use pulsebeam_runtime::time::Instant;

use crate::bitrate::Bitrate;
use crate::entity::PublicationKey;
use crate::ice_tcp::{FlowRoute, TcpFlow, TcpFlowId, TcpPacket};
use crate::media::{
    ChannelId, KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Pt, Rid, Ssrc,
};
use crate::participant::batcher::{Handoff, NetworkEgress, OwnedPacketQueue};
use crate::participant::signaling::{SignalingOutput, SignalingWrite, SignalingWriter};
use crate::rtp::{Codec as RtpCodec, Frequency, MediaTime, RtpExtensions, RtpPacket};
use crate::track::SourceEncoding;

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

pub(super) fn native_data_config(
    cid: ChannelId,
    lane: crate::track::DataLane,
) -> pulsebeam_rtc::DataChannelConfig {
    let ordered = lane == crate::track::DataLane::Ordered;
    pulsebeam_rtc::DataChannelConfig {
        id: Some(cid),
        label: "".into(),
        protocol: "".into(),
        negotiated: true,
        ordered,
        reliability: if ordered {
            pulsebeam_rtc::DataReliability::Reliable
        } else {
            pulsebeam_rtc::DataReliability::MaxRetransmits(0)
        },
        priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
    }
}

pub(super) fn fabric_data_config(
    parameters: pulsebeam_proto::state::SctpDataV1Parameters,
) -> Option<pulsebeam_rtc::DataChannelConfig> {
    parameters.format()?;
    let stream = u16::try_from(parameters.stream_id?).ok()?;
    Some(pulsebeam_rtc::DataChannelConfig {
        protocol: "pulsebeam.data.v1".into(),
        ..native_data_config(ChannelId::from_value(stream), crate::track::DataLane::Lossy)
    })
}

fn sdp_fabric_profile(
    codec: pulsebeam_rtc::CodecProfile,
) -> Option<pulsebeam_core::fabric::Profile> {
    use pulsebeam_core::fabric::Profile;
    use pulsebeam_rtc::CodecProfile;
    match codec {
        CodecProfile::Opus => Some(Profile::Opus),
        CodecProfile::H264 {
            packetization_mode: 1,
            ..
        } => Some(Profile::H264),
        _ => None,
    }
}

fn tcp_frame_payload(frame: &[u8]) -> Option<&[u8]> {
    let length = u16::from_be_bytes(frame.get(..2)?.try_into().ok()?);
    let payload = frame.get(2..)?;
    (usize::from(length) == payload.len()).then_some(payload)
}

fn egress_extensions(
    values: RtpExtensions,
    writer: &mut pulsebeam_core::dd::DependencyDescriptorWriter,
    mappings: &[pulsebeam_rtc::RtpExtensionInfo],
) -> Result<Vec<RtpExtension>, pulsebeam_rtc::WriteError> {
    let mut extensions = values
        .opaque
        .iter()
        .cloned()
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
                    | crate::rtp::PLAYOUT_DELAY_URI
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
        let bytes = values
            .write_dependency_descriptor(writer, &mut descriptor)
            .and_then(|len| descriptor.get(..len));
        if let Some(bytes) = bytes {
            extensions.push(RtpExtension {
                uri: mapping.uri.clone(),
                value: rtc_bytes(bytes),
            });
        }
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
    Keyframe {
        mid: Mid,
        encoding: SourceEncoding,
        kind: KeyframeRequestKind,
    },
}

pub(crate) struct RtpWriteCommand {
    pub(crate) pkt: RtpPacket,
    pub(crate) publication_id: PublicationKey,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) ssrc: Ssrc,
    pub(crate) pt: Pt,
    pub(crate) kind: MediaKind,
    pub(crate) now: Instant,
}

pub(crate) struct RtpIngress {
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) packet: RtpPacket,
}

pub(crate) enum AppliedMutation {
    KeyframeUnavailable { mid: Mid, encoding: SourceEncoding },
    KeyframeRequested { mid: Mid, encoding: SourceEncoding },
    RtpNotWritten,
    RtpWritten,
}

struct PendingIngress {
    batch: RecvPacketBatch,
    source_shard: crate::id::ShardId,
}
pub(super) type AuthenticatedIngressObserver<'a> =
    dyn for<'packet> FnMut(pulsebeam_rtc::AuthenticatedRtp<'packet>) + 'a;

struct ScopedMedia {
    id: u64,
    branch: crate::shard::router::FabricLocalBranch,
    ingress: Option<std::num::NonZeroU64>,
    transmit: pulsebeam_rtc::Transmit,
    tcp_flow: Option<TcpFlowId>,
}

struct InputObservation {
    path: AuthenticatedPath,
    source_shard: crate::id::ShardId,
    tcp_flow: Option<TcpFlowId>,
}
struct InboundPayload {
    payload_type: u8,
    codec: RtpCodec,
    frequency: Frequency,
}
struct Encoding {
    mid: Mid,
    rid: Option<Rid>,
    payloads: Box<[InboundPayload]>,
    sequence: Option<u64>,
    timestamp: Option<u64>,
    pending_fabric_report: Option<pulsebeam_rtc::SenderReport>,
    fabric_claimed: bool,
}
struct Sender {
    ssrc: Ssrc,
    pt: Pt,
    sequence_origin: Option<u64>,
    highest_accepted: Option<u64>,
    source: Option<(PublicationKey, Ssrc)>,
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
}

pub(crate) struct RtcDriver {
    rtc: RtcConnection,
    session: SessionInfo,
    senders: HashMap<Mid, Sender>,
    encodings: HashMap<Ssrc, Encoding>,
    input_observation: Option<InputObservation>,
    udp_packets: OwnedPacketQueue,
    scoped_media: Option<ScopedMedia>,
    tcp_flows: HashMap<SocketAddr, TcpFlow>,
    pending_tcp: VecDeque<TcpPacket>,
    pending_ingress: VecDeque<PendingIngress>,
    pending_timeout: Option<Instant>,
    pending_mutations: VecDeque<TransportMutation>,
    signaling: SignalingWriter,
    rtc_deadline: Option<Instant>,
    rtc_clock: Instant,
    rtc_needs_drain: bool,
    exited: bool,
    #[cfg(debug_assertions)]
    egress_guard: crate::rtp::egress_guard::EgressGuard,
    #[cfg(feature = "sim")]
    pub(crate) sim_span: tracing::Span,
}

impl RtcDriver {
    pub(crate) fn new(
        accepted: AcceptedConnection,
        udp_gso_size: usize,
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
            input_observation: None,
            udp_packets: OwnedPacketQueue::with_capacity(udp_gso_size),
            scoped_media: None,
            tcp_flows: HashMap::new(),
            pending_tcp: VecDeque::new(),
            pending_ingress: VecDeque::new(),
            pending_timeout: None,
            pending_mutations: VecDeque::new(),
            signaling: SignalingWriter::default(),
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

    pub(crate) fn bind_tcp_flow(&mut self, flow: TcpFlow) -> bool {
        if !flow.is_open() {
            return false;
        }
        self.tcp_flows.insert(flow.route.remote, flow);
        true
    }

    pub(crate) fn retire_tcp_flow(&mut self, route: FlowRoute) {
        if self
            .tcp_flows
            .get(&route.remote)
            .is_some_and(|flow| flow.route == route)
        {
            self.tcp_flows.remove(&route.remote);
        }
    }

    pub(crate) fn enqueue_tcp(&mut self, packet: TcpPacket) -> bool {
        if !self
            .tcp_flows
            .get(&packet.route.remote)
            .is_some_and(|flow| flow.route == packet.route)
        {
            return false;
        }
        if self.pending_tcp.len() >= 8 {
            self.retire_tcp_flow(packet.route);
            metrics::counter!("participant_tcp_ingress_close", "reason" => "full").increment(1);
            return false;
        }
        self.pending_tcp.push_back(packet);
        true
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
    pub(crate) fn apply_next_mutation(&mut self) -> Option<AppliedMutation> {
        Some(match self.pending_mutations.pop_front()? {
            TransportMutation::Keyframe {
                mid,
                encoding,
                kind,
            } => {
                if self.request_encoding_keyframe(mid, encoding, kind) {
                    AppliedMutation::KeyframeRequested { mid, encoding }
                } else {
                    AppliedMutation::KeyframeUnavailable { mid, encoding }
                }
            }
        })
    }

    pub(super) fn staged_handoff(
        &self,
        participant: crate::keys::ParticipantHandle,
    ) -> Option<Handoff> {
        let scoped = self.scoped_media.as_ref()?;
        Some(Handoff {
            participant,
            branch: scoped.branch,
            ingress: scoped.ingress,
            id: scoped.id,
        })
    }

    pub(super) fn dispatch_handoff(&mut self, handoff: Handoff, authorized: bool) {
        let Some(scoped) = self.scoped_media.take_if(|media| {
            media.id == handoff.id
                && media.branch == handoff.branch
                && media.ingress == handoff.ingress
        }) else {
            return;
        };
        if !authorized || !self.rtc.handoff_pending(handoff.id) {
            self.resolve_handoff(handoff.id, false);
            return;
        }
        match scoped.transmit.target {
            TransmitTarget::Udp { remote, .. } => {
                if !self.udp_packets.push_with_handoff(
                    remote,
                    scoped.transmit.payload.to_vec(),
                    Some(handoff),
                ) {
                    self.resolve_handoff(handoff.id, false);
                }
            }
            TransmitTarget::IceTcp { remote, .. } => {
                let accepted = tcp_frame_payload(&scoped.transmit.payload).is_some_and(|payload| {
                    !payload.is_empty() && self.send_tcp(remote, scoped.tcp_flow, payload)
                });
                self.resolve_handoff(handoff.id, accepted);
            }
        }
    }

    fn tcp_flow_for(&self, local: SocketAddr, remote: SocketAddr) -> Option<TcpFlowId> {
        self.input_observation
            .as_ref()
            .filter(|observation| {
                observation.path.protocol == NetworkProtocol::IceTcp
                    && observation.path.local == local
                    && observation.path.remote == remote
            })
            .map_or_else(
                || self.tcp_flows.get(&remote).map(|flow| flow.route.id),
                |observation| observation.tcp_flow,
            )
    }

    fn send_tcp(&mut self, remote: SocketAddr, flow: Option<TcpFlowId>, payload: &[u8]) -> bool {
        let Some(handle) = self
            .tcp_flows
            .get(&remote)
            .filter(|handle| Some(handle.route.id) == flow)
        else {
            metrics::counter!("participant_tcp_egress_dropped_bytes", "reason" => "missing_flow")
                .increment(payload.len() as u64);
            return false;
        };
        let id = handle.route.id;
        if handle.try_send(payload) {
            return true;
        }
        tracing::debug!(%remote, ?id, "ICE-TCP egress full or closed; retiring flow");
        self.tcp_flows.remove(&remote);
        false
    }

    pub(super) fn network_pending(&self) -> bool {
        !self.udp_packets.is_empty()
    }

    #[cfg(test)]
    pub(super) fn fabric_primary_progress(&self, mid: Mid) -> Option<u64> {
        self.rtc.rtp_egress_progress(mid.as_str(), None)?.0
    }

    #[cfg(all(test, feature = "transport-conformance"))]
    pub(super) fn connection_stats(&self) -> pulsebeam_rtc::ConnectionStats {
        self.rtc.stats().connection
    }

    pub(super) fn handoff_pending(&self, id: u64) -> bool {
        self.rtc.handoff_pending(id)
    }

    pub(super) fn resolve_handoff(&mut self, id: u64, accepted: bool) -> bool {
        let resolved = self.rtc.resolve_handoff(id, accepted);
        self.rtc_needs_drain |= resolved;
        resolved
    }

    pub(crate) fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                super::batcher::AppendStatus::Drained => break,
                super::batcher::AppendStatus::Full if !egress.flush() => break,
                super::batcher::AppendStatus::Full => {}
            }
        }
    }
    pub(super) fn set_sender_report_time(&mut self, clock: &crate::clock::reference::ShardClock) {
        let at = self.rtc_clock.into_std();
        if self.rtc.sender_report_due(at) {
            self.rtc
                .set_sender_report_time(at, clock.sender_report_time(self.rtc_clock));
        }
    }

    pub(super) fn poll_output(
        &mut self,
        observer: Option<&mut AuthenticatedIngressObserver<'_>>,
    ) -> TransportPollOutput {
        if self.udp_packets.is_full() {
            return TransportPollOutput::Pending;
        }
        let now = self.rtc_clock.into();
        let output = match (observer, &self.input_observation) {
            (Some(observer), Some(input)) => self.rtc.poll_with_rtp_observer(
                now,
                &mut |packet: pulsebeam_rtc::AuthenticatedRtp<'_>| {
                    if packet.arrival == input.path.arrival {
                        observer(packet);
                    }
                },
            ),
            _ => self.rtc.poll(now),
        };
        match output {
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
            Output::Handoff {
                id,
                scope,
                transmit,
            } => {
                let Some((scope, branch)) = scope.and_then(|scope| {
                    crate::shard::router::FabricLocalBranch::from_scope(scope)
                        .map(|branch| (scope, branch))
                }) else {
                    self.resolve_handoff(id, false);
                    return TransportPollOutput::Pending;
                };
                let tcp_flow = match transmit.target {
                    TransmitTarget::IceTcp { local, remote } => self.tcp_flow_for(local, remote),
                    _ => None,
                };
                self.scoped_media = Some(ScopedMedia {
                    id,
                    branch,
                    ingress: scope.provenance(),
                    transmit,
                    tcp_flow,
                });
                TransportPollOutput::Pending
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
                        // The I/O task owns RFC 4571 framing and all partial writes.
                        match tcp_frame_payload(&tx.payload) {
                            Some([]) => {}
                            Some(payload) => {
                                // Deferred replies belong to the original socket, even after EOF.
                                let flow = self.tcp_flow_for(local, remote);
                                self.send_tcp(remote, flow, payload);
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
    pub(super) fn open_native_data(
        &mut self,
        cid: ChannelId,
        lane: crate::track::DataLane,
    ) -> Result<pulsebeam_rtc::DataChannelInstallation, super::policy::ExecutionFailureKind> {
        use super::policy::ExecutionFailureKind;
        let Some(capacity) = self.rtc.data_channel_capacity() else {
            tracing::debug!(?cid, "native data association capacity unavailable");
            return Err(ExecutionFailureKind::Conflict);
        };
        if cid.value() == 0 || cid.value() >= capacity {
            tracing::debug!(
                ?cid,
                capacity,
                "native data stream outside association capacity"
            );
            return Err(ExecutionFailureKind::Conflict);
        }
        let config = native_data_config(cid, lane);
        match self.install_data_channel(config) {
            Ok(installation) => Ok(installation),
            Err(reason) => {
                tracing::debug!(?cid, ?reason, "native data RTC open rejected");
                Err(match reason {
                    pulsebeam_rtc::WriteError::WouldBlock => ExecutionFailureKind::Capacity,
                    _ => ExecutionFailureKind::Conflict,
                })
            }
        }
    }

    pub(crate) fn discard_mutations(&mut self) {
        self.pending_mutations.clear();
    }

    pub(crate) fn install_data_channel(
        &mut self,
        config: pulsebeam_rtc::DataChannelConfig,
    ) -> Result<pulsebeam_rtc::DataChannelInstallation, pulsebeam_rtc::WriteError> {
        let installation = self
            .rtc
            .install_data_channel(self.rtc_clock.into(), config)
            .map_err(|(reason, _)| reason)?;
        self.rtc_needs_drain = true;
        Ok(installation)
    }

    pub(crate) fn data_channel_installation(
        &self,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<pulsebeam_rtc::DataChannelInstallationState> {
        self.rtc.data_channel_installation(installation)
    }

    pub(super) fn install_fabric_data(
        &mut self,
        parameters: pulsebeam_proto::state::SctpDataV1Parameters,
    ) -> Result<pulsebeam_rtc::DataChannelInstallation, pulsebeam_rtc::WriteError> {
        let config =
            fabric_data_config(parameters).ok_or(pulsebeam_rtc::WriteError::InvalidState)?;
        let cap = std::num::NonZeroUsize::new(pulsebeam_proto::data_record::MAX_RECORD_BYTES)
            .ok_or(pulsebeam_rtc::WriteError::InvalidState)?;
        let installation = self
            .rtc
            .install_bounded_data_channel(self.rtc_clock.into(), config, cap)
            .map_err(|(reason, _)| reason)?;
        self.rtc_needs_drain = true;
        Ok(installation)
    }

    pub(super) fn supports_fabric_data(
        &self,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> bool {
        self.rtc
            .supports_unordered_data(installation, pulsebeam_proto::data_record::MAX_RECORD_BYTES)
    }

    pub(crate) fn cancel_data_channel_installation(
        &mut self,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) {
        let _ = self
            .rtc
            .cancel_data_channel_installation(self.rtc_clock.into(), installation);
        self.rtc_needs_drain = true;
    }

    fn prepare_rtp_senders<'a>(
        bindings: impl ExactSizeIterator<Item = &'a pulsebeam_rtc::RtpBinding>,
    ) -> Result<Vec<(Mid, Sender)>, pulsebeam_rtc::RtpConfigurationError> {
        let mut outbound = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let [encoding] = binding.encodings.as_slice() else {
                return Err(pulsebeam_rtc::RtpConfigurationError::Unsupported);
            };
            if encoding.rid.is_some() {
                return Err(pulsebeam_rtc::RtpConfigurationError::Unsupported);
            }
            let mid = Mid::new(&binding.mid)
                .map_err(|_| pulsebeam_rtc::RtpConfigurationError::Unsupported)?;
            outbound.push((
                mid,
                Sender {
                    ssrc: encoding.primary.into(),
                    pt: binding.codec.payload_type.into(),
                    sequence_origin: None,
                    highest_accepted: None,
                    source: None,
                    descriptor_writer: Default::default(),
                    extensions: binding
                        .extensions
                        .iter()
                        .map(|extension| pulsebeam_rtc::RtpExtensionInfo {
                            id: extension.id,
                            uri: extension.uri.as_str().into(),
                        })
                        .collect(),
                },
            ));
        }
        Ok(outbound)
    }

    fn apply_rtp_senders(
        &mut self,
        outbound: Vec<(Mid, Sender)>,
        removed: &std::collections::BTreeSet<u32>,
    ) {
        self.senders
            .retain(|_, sender| !removed.contains(&u32::from(sender.ssrc)));
        for (mid, sender) in outbound {
            if let Some(previous) = self.senders.get_mut(&mid)
                && previous.ssrc == sender.ssrc
                && previous.pt == sender.pt
            {
                previous.extensions = sender.extensions;
            } else {
                self.senders.insert(mid, sender);
            }
        }
        self.rtc_needs_drain = true;
    }

    pub(crate) fn update_compiled_rtp(
        &mut self,
        patch: pulsebeam_rtc::CompiledRtpConfigurationPatch,
    ) -> Result<(), pulsebeam_rtc::RtpConfigurationError> {
        let outbound = Self::prepare_rtp_senders(
            patch
                .outbound
                .iter()
                .map(pulsebeam_rtc::CompiledRtpBinding::description),
        )?;
        let removed = patch.remove_encodings.clone();
        self.rtc.update_compiled_rtp(patch)?;
        self.apply_rtp_senders(outbound, &removed);
        Ok(())
    }

    pub(crate) fn set_rtp_encoding_active(
        &mut self,
        primary: u32,
        active: bool,
    ) -> Result<(), pulsebeam_rtc::RtpConfigurationError> {
        self.rtc.set_rtp_encoding_active(primary, active)?;
        self.rtc_needs_drain = true;
        Ok(())
    }

    pub(crate) fn discard_rtp(&mut self, mid: Mid) -> bool {
        self.rtc.discard_pending_rtp(mid.as_str()).is_ok()
    }

    pub(crate) fn discard_keyframes(&mut self, mid: Mid) {
        self.pending_mutations.retain(|mutation| !matches!(mutation, TransportMutation::Keyframe { mid: pending, .. } if *pending == mid));
    }

    pub(crate) fn close_native_data(&mut self, cid: ChannelId) {
        let _ = self.rtc.close_data_channel(self.rtc_clock.into(), cid);
        self.rtc_needs_drain = true;
    }

    pub(crate) fn signaling_pending(&self) -> bool {
        self.signaling.has_pending()
    }

    pub(crate) fn offer_signaling(
        &mut self,
        output: SignalingOutput,
    ) -> Result<(), SignalingOutput> {
        self.signaling.offer(output)
    }

    pub(crate) fn poll_signaling(&mut self) -> SignalingWrite {
        let result = self.signaling.poll(&mut self.rtc, self.rtc_clock);
        self.rtc_needs_drain |= matches!(
            result,
            SignalingWrite::Admitted(_) | SignalingWrite::Blocked
        );
        result
    }

    pub(super) fn write_installed_binary(
        &mut self,
        installation: pulsebeam_rtc::DataChannelInstallation,
        bytes: &[u8],
    ) -> Result<(), pulsebeam_rtc::WriteError> {
        self.rtc
            .send_installed_data(
                self.rtc_clock.into(),
                installation,
                DataMessage::Binary(rtc_bytes(bytes)),
            )
            .map_err(|rejected| rejected.reason)?;
        self.rtc_needs_drain = true;
        Ok(())
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
    pub(crate) fn register_encoding(&mut self, info: pulsebeam_rtc::EncodingInfo) {
        if self.encodings.len() >= MAX_ENCODINGS && !self.encodings.contains_key(&info.ssrc.into())
        {
            return;
        }
        self.register_owned_encoding(info)
    }

    // Native callers have already reserved per-resource execution credit and
    // checked this SSRC against the admitted source's index.
    pub(crate) fn register_owned_encoding(&mut self, info: pulsebeam_rtc::EncodingInfo) {
        let (Ok(mid), Ok(rid)) = (
            Mid::new(&info.mid),
            info.rid.as_deref().map(Rid::new).transpose(),
        ) else {
            return;
        };
        let payloads: Box<[_]> = info
            .payloads
            .iter()
            .filter_map(|payload| {
                let codec = match (info.kind, payload.name.as_ref()) {
                    (pulsebeam_rtc::MediaKind::Audio, "opus") => RtpCodec::Opus,
                    (pulsebeam_rtc::MediaKind::Video, "h264") => RtpCodec::H264,
                    _ => return None,
                };
                Some(InboundPayload {
                    payload_type: payload.payload_type,
                    codec,
                    frequency: Frequency::from_nonzero(std::num::NonZeroU32::new(
                        payload.clock_rate,
                    )?),
                })
            })
            .collect();
        if payloads.is_empty() {
            return;
        }
        self.encodings.entry(info.ssrc.into()).or_insert(Encoding {
            mid,
            rid,
            payloads,
            sequence: None,
            timestamp: None,
            pending_fabric_report: None,
            fabric_claimed: false,
        });
    }
    pub(super) fn rtp_receive_binding(
        &self,
        primary: u32,
    ) -> Option<pulsebeam_rtc::RtpReceiveBinding<'_>> {
        self.rtc.rtp_receive_binding(primary)
    }

    pub(super) fn sdp_fabric_receive_binding(
        &self,
        mid: Mid,
    ) -> Option<(
        pulsebeam_core::fabric::Profile,
        pulsebeam_rtc::RtpReceiveBinding<'_>,
    )> {
        let media = self
            .session
            .inbound_media
            .iter()
            .find(|media| media.mid.as_ref() == mid.as_str())?;
        let [primary] = media.primary_ssrcs.as_ref() else {
            return None;
        };
        let [codec] = media.codecs.as_ref() else {
            return None;
        };
        if !media.rids.is_empty() {
            return None;
        }
        let binding = self.rtc.sdp_receive_binding(*primary)?;
        let profile = sdp_fabric_profile(codec.profile)?;
        if profile == pulsebeam_core::fabric::Profile::H264 && media.receives_dependency_descriptor
        {
            return None;
        }
        (binding.mid == mid.as_str()
            && binding.rid.is_none()
            && binding.primary_codec.payload_type == codec.payload_type)
            .then_some((profile, binding))
    }

    pub(super) fn sdp_fabric_send_profile(
        &self,
        mid: Mid,
    ) -> Option<pulsebeam_core::fabric::Profile> {
        let sender = self
            .session
            .senders
            .iter()
            .find(|sender| sender.mid.as_ref() == mid.as_str())?;
        sdp_fabric_profile(sender.codec.profile)
    }

    pub(super) fn local_rtp_output_form(
        &self,
        mid: Mid,
        primary: u32,
    ) -> Option<crate::rtp::fabric_ingress::ExtensionForm> {
        use crate::rtp::fabric_ingress::ExtensionForm;
        use pulsebeam_rtc::RtpExtensionForm;
        Some(
            match self
                .rtc
                .rtp_egress_extension_form(mid.as_str(), Some(primary))?
            {
                RtpExtensionForm::OneByte => ExtensionForm::OneByte,
                RtpExtensionForm::TwoByte => ExtensionForm::TwoByte,
                RtpExtensionForm::Mixed => ExtensionForm::Mixed,
            },
        )
    }

    pub(super) fn fabric_rtp_answer_fid(&self, mid: Mid, primary: u32) -> Option<[u32; 2]> {
        self.rtc.rtp_egress_answer_fid(mid.as_str(), primary)
    }

    pub(super) fn fabric_rtp_extension_id(&self, mid: Mid, primary: u32, uri: &str) -> Option<u8> {
        self.rtc
            .rtp_egress_extension_id(mid.as_str(), Some(primary), uri)
    }

    pub(super) fn fabric_rtp_sender_is_fresh(
        &self,
        output: crate::rtp::fabric_egress::OutputLeg,
    ) -> bool {
        self.rtc
            .rtp_egress_progress(output.mid.as_str(), output.primary)
            == Some((None, None))
    }

    pub(super) fn retire_fabric_rtp_scope(&mut self, scope: pulsebeam_rtc::MediaScope) {
        if self.rtc.retire_rtp_scope(scope).is_ok() {
            self.scoped_media
                .take_if(|media| !self.rtc.handoff_pending(media.id));
            self.rtc_needs_drain = true;
        }
    }

    pub(super) fn discard_pending_fabric_rtp(&mut self, scope: pulsebeam_rtc::MediaScope) -> bool {
        if self.rtc.discard_pending_rtp_scope(scope).is_err() {
            return false;
        }
        self.scoped_media
            .take_if(|media| !self.rtc.handoff_pending(media.id));
        self.rtc_needs_drain = true;
        true
    }

    pub(super) fn queue_fabric_rtp(
        &mut self,
        leg: &mut crate::rtp::fabric_egress::SenderLeg<crate::shard::router::FabricLocalBranch>,
        branch: crate::shard::router::FabricLocalBranch,
        source: crate::rtp::fabric_egress::SourceMetadata,
        packet: crate::rtp::fabric_egress::CandidatePacket<'_, '_>,
    ) -> Result<(), crate::rtp::fabric_egress::AdmissionError> {
        use crate::rtp::fabric_egress::AdmissionError;

        // Let already protected UDP datagrams cross the adapter before
        // committing a cut. At this point they cannot be scoped by sender.
        if leg.cut_pending() && !self.udp_packets.is_empty() {
            return Err(AdmissionError::Rtc(pulsebeam_rtc::WriteError::WouldBlock));
        }
        leg.admit(&mut self.rtc, self.rtc_clock.into(), branch, source, packet)?;
        self.rtc_needs_drain = true;
        Ok(())
    }

    pub(crate) fn retire_encoding(&mut self, ssrc: u32) {
        self.encodings.remove(&ssrc.into());
    }
    pub(crate) fn convert_rtp(
        &mut self,
        rtp: pulsebeam_rtc::RtpPacket,
        mid: Mid,
        rid: Option<Rid>,
    ) -> Option<RtpIngress> {
        let encoding = self.encodings.get_mut(&rtp.header.ssrc.into())?;
        if encoding.mid != mid || encoding.rid.is_some_and(|known| Some(known) != rid) {
            return None;
        }
        let payload = encoding
            .payloads
            .iter()
            .find(|payload| payload.payload_type == rtp.header.payload_type)?;
        let frequency = payload.frequency;
        let kind = payload.codec;
        let sequence = extend_serial(encoding.sequence, u64::from(rtp.header.sequence), 16);
        let timestamp = extend_serial(encoding.timestamp, u64::from(rtp.header.timestamp), 32);
        encoding.sequence = Some(encoding.sequence.unwrap_or(sequence).max(sequence));
        encoding.timestamp = Some(encoding.timestamp.unwrap_or(timestamp).max(timestamp));
        let packet = RtpPacket::from_rtc(rtp, kind, frequency, sequence, timestamp, rid)?;
        if encoding.rid.is_none() {
            encoding.rid = rid;
        }
        Some(RtpIngress { mid, rid, packet })
    }
    pub(super) fn defer_fabric_sender_report(&mut self, report: pulsebeam_rtc::SenderReport) {
        if let Some(encoding) = self.encodings.get_mut(&report.ssrc.into())
            && !encoding.fabric_claimed
            && encoding
                .pending_fabric_report
                .is_none_or(|old| old.arrival < report.arrival)
        {
            encoding.pending_fabric_report = Some(report);
        }
    }

    pub(super) fn claim_fabric_sender_report(
        &mut self,
        ssrc: u32,
        mid: Mid,
    ) -> Option<pulsebeam_rtc::SenderReport> {
        let encoding = self.encodings.get_mut(&ssrc.into())?;
        if encoding.mid != mid || std::mem::replace(&mut encoding.fabric_claimed, true) {
            return None;
        }
        encoding.pending_fabric_report.take()
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
        !self.pending_ingress.is_empty() || !self.pending_tcp.is_empty()
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
    fn request_encoding_keyframe(
        &mut self,
        mid: Mid,
        encoding: SourceEncoding,
        kind: KeyframeRequestKind,
    ) -> bool {
        match encoding {
            SourceEncoding::Rid(rid) => self.request_keyframe(KeyframeRequest { mid, rid, kind }),
            SourceEncoding::Ssrc(ssrc) => {
                let accepted = self
                    .encodings
                    .get(&ssrc)
                    .is_some_and(|encoding| encoding.mid == mid)
                    && self.rtc.request_keyframe(*ssrc).is_ok();
                self.rtc_needs_drain |= accepted;
                accepted
            }
        }
    }

    pub(super) fn request_fabric_refresh(&mut self, ssrc: u32) -> bool {
        let accepted = self.rtc.request_keyframe(ssrc).is_ok();
        self.rtc_needs_drain |= accepted;
        accepted
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
        let source = (command.publication_id, command.pkt.ssrc);
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
            marker,
            payload: rtc_bytes(&command.pkt.payload),
            extensions,
        };
        if let Err(rejected) =
            self.rtc
                .write_rtp(self.rtc_clock.into(), command.mid.as_str(), packet)
        {
            metrics::counter!("participant_rtp_write_rejected").increment(1);
            tracing::debug!(reason = ?rejected.reason, "RTC rejected primary RTP");
            return AppliedMutation::RtpNotWritten;
        }
        if command.kind == MediaKind::Video {
            tracing::trace!(target: crate::log::TARGET_VIDEO, sequence, timestamp, marker, "Writing RTP packet");
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
        let (protocol, local, remote, received_at, tcp_flow, source_shard, input) =
            if let Some(pending) = self.pending_ingress.front_mut() {
                let source_shard = pending.source_shard;
                let batch = &mut pending.batch;
                let (remote, local, received_at) = (batch.src, batch.dst, batch.received_at);
                let Some(packet) = batch.next_packet() else {
                    self.pending_ingress.pop_front();
                    return IngressResult::Empty;
                };
                (
                    NetworkProtocol::Udp,
                    local,
                    remote,
                    received_at,
                    None,
                    source_shard,
                    NetworkInput::Udp {
                        local,
                        remote,
                        ecn: None,
                        payload: rtc_bytes(packet),
                    },
                )
            } else if let Some(packet) = self.pending_tcp.pop_front() {
                let FlowRoute {
                    local,
                    remote,
                    id,
                    address,
                } = packet.route;
                let Ok(length) = u16::try_from(packet.packet.len()) else {
                    return IngressResult::Received;
                };
                let mut frame = Vec::with_capacity(packet.packet.len().saturating_add(2));
                frame.extend_from_slice(&length.to_be_bytes());
                frame.extend_from_slice(&packet.packet);
                (
                    NetworkProtocol::IceTcp,
                    local,
                    remote,
                    packet.received_at,
                    Some(id),
                    address.shard(),
                    NetworkInput::IceTcp {
                        local,
                        remote,
                        frame: rtc_bytes(&frame),
                    },
                )
            } else {
                return IngressResult::Empty;
            };
        let arrival = received_at.monotonic;
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
        self.signaling.clear();
        self.pending_ingress.clear();
        self.pending_mutations.clear();
        self.pending_timeout = None;
        self.input_observation = None;
        self.udp_packets.discard();
        self.pending_tcp.clear();
        self.tcp_flows.clear();
        self.rtc.abort();
        self.rtc_needs_drain = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_tcp_ingress_never_moves_its_output_to_a_replacement_flow() {
        let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.16");
        pulsebeam_runtime::testing::run_local(host, async move {
            let (accepted, _, binding) =
                crate::control::application_only_test_connection_and_binding();
            let local = "127.0.0.1:6000".parse().unwrap();
            let remote = "127.0.0.1:5000".parse().unwrap();
            let address = crate::route::NodeTransportAddress::new(
                crate::route::TransportRoute::new(crate::id::ShardId::new(0), 7),
                1,
            );
            let mut transport = RtcDriver::new(
                accepted,
                8,
                Instant::now(),
                #[cfg(feature = "sim")]
                tracing::Span::none(),
            );
            let (flow, mut original_output) = crate::ice_tcp::fixture(address, local, remote, 4);
            let original = flow.route;
            assert!(transport.bind_tcp_flow(flow));
            for _ in 0..2 {
                assert!(transport.enqueue_tcp(TcpPacket::fixture(original, &binding)));
            }
            let receive_and_drain = |transport: &mut RtcDriver| {
                let now = Instant::now();
                transport.advance_clock(now, now);
                assert!(matches!(
                    transport.receive_pending(now),
                    IngressResult::Received
                ));
                loop {
                    match transport.poll_output(None) {
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
            assert!(original_output.try_recv().is_ok());
            transport.retire_tcp_flow(original);
            let (replacement, mut replacement_output) =
                crate::ice_tcp::fixture(address, local, remote, 4);
            let replacement_route = replacement.route;
            assert!(transport.bind_tcp_flow(replacement));
            receive_and_drain(&mut transport);
            assert!(replacement_output.try_recv().is_err());
            // Late close and frame events cannot retire or feed a successor.
            transport.retire_tcp_flow(original);
            assert!(!transport.enqueue_tcp(TcpPacket::fixture(original, &binding)));
            assert_eq!(
                transport.tcp_flows.get(&remote).unwrap().route,
                replacement_route
            );
            assert!(transport.tcp_flows.get(&remote).unwrap().try_send(b"new"));
            assert_eq!(replacement_output.try_recv().unwrap(), b"new");
            for _ in 0..pulsebeam_runtime::net::BATCH_SIZE {
                assert!(transport.udp_packets.push_back(remote, vec![1]));
            }
            assert!(!transport.udp_packets.push_back(remote, vec![2]));
            assert!(matches!(
                transport.poll_output(None),
                TransportPollOutput::Pending
            ));
            transport.disconnect();
            assert!(matches!(
                transport.poll_output(None),
                TransportPollOutput::Closed(_)
            ));
            assert!(transport.pending_ingress.is_empty());
            assert!(transport.tcp_flows.is_empty());
        });
    }

    #[test]
    fn ingress_uses_accepted_encoding_formats_not_bootstrap_media() {
        let accepted = crate::control::application_only_test_connection();
        let mut transport = RtcDriver::new(
            accepted,
            8,
            Instant::now(),
            #[cfg(feature = "sim")]
            tracing::Span::none(),
        );
        assert!(transport.session.inbound_media.is_empty());
        let mid = Mid::from("dynamic");
        let info = |ssrc, payload_type, name: &str, kind| pulsebeam_rtc::EncodingInfo {
            mid: mid.as_str().into(),
            ssrc,
            kind,
            rid: None,
            payloads: Box::new([pulsebeam_rtc::RtpPayloadInfo {
                payload_type,
                name: name.into(),
                clock_rate: 48_000,
            }]),
        };
        let report = pulsebeam_rtc::SenderReport {
            arrival: std::time::Instant::now(),
            ssrc: 0,
            ntp_seconds: 1,
            ntp_fraction: 0,
            rtp_timestamp: 2000,
        };
        transport.defer_fabric_sender_report(report);
        transport.register_encoding(info(0, 111, "opus", MediaKind::Audio));
        // SSRC-only reports from before registration do not enter Fabric.
        assert!(
            transport
                .encodings
                .get(&0.into())
                .unwrap()
                .pending_fabric_report
                .is_none()
        );
        transport.defer_fabric_sender_report(report);
        assert_eq!(
            transport.claim_fabric_sender_report(0, Mid::from("other")),
            None
        );
        assert_eq!(transport.claim_fabric_sender_report(0, mid), Some(report));
        transport.defer_fabric_sender_report(report);
        assert_eq!(transport.claim_fabric_sender_report(0, mid), None);
        let packet = pulsebeam_rtc::RtpPacket {
            arrival: report.arrival,
            header: pulsebeam_rtc::RtpHeader {
                sequence: 65535,
                timestamp: 2400,
                ssrc: 0,
                payload_type: 111,
                marker: true,
            },
            payload: Bytes::from_static(&[0xf8, 0xff, 0xfe]),
            extensions: vec![],
        };
        let ingress = transport.convert_rtp(packet.clone(), mid, None).unwrap();
        assert_eq!(
            ingress.packet.rtp_ts,
            MediaTime::new((1u64 << 32) + 2400, Frequency::FORTY_EIGHT_KHZ)
        );
        assert!(
            transport
                .convert_rtp(packet.clone(), Mid::from("other"), None)
                .is_none()
        );
        let mut wrong_pt = packet.clone();
        wrong_pt.header.payload_type = 112;
        let rid = Rid::new("late").unwrap();
        assert!(transport.convert_rtp(wrong_pt, mid, Some(rid)).is_none());
        let ingress = transport
            .convert_rtp(packet.clone(), mid, Some(rid))
            .unwrap();
        assert_eq!(ingress.rid, Some(rid));
        assert!(transport.convert_rtp(packet.clone(), mid, None).is_none());
        assert!(
            transport
                .convert_rtp(packet.clone(), mid, Some(Rid::new("other").unwrap()))
                .is_none()
        );
        transport.retire_encoding(0);
        transport.register_encoding(info(1, 112, "opus", MediaKind::Audio));
        assert!(transport.convert_rtp(packet.clone(), mid, None).is_none());
        let mut replacement = packet;
        replacement.header.ssrc = 1;
        replacement.header.payload_type = 112;
        assert!(transport.convert_rtp(replacement, mid, None).is_some());
        transport.defer_fabric_sender_report(pulsebeam_rtc::SenderReport { ssrc: 1, ..report });
        transport.retire_encoding(1);
        transport.register_encoding(info(1, 112, "opus", MediaKind::Audio));
        assert_eq!(transport.claim_fabric_sender_report(1, mid), None);
        transport.register_encoding(info(2, 96, "vp8", MediaKind::Video));
        assert!(!transport.encodings.contains_key(&2.into()));
    }

    fn extensions(values: RtpExtensions) -> Vec<RtpExtension> {
        let mappings = [pulsebeam_rtc::RtpExtensionInfo {
            id: 1,
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
        }];
        egress_extensions(values, &mut Default::default(), &mappings).unwrap()
    }

    #[test]
    fn fresh_default_egress_does_not_inherit_ingress_delay() {
        let ingress = RtpExtensions {
            opaque: vec![RtpExtension {
                uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
                value: Bytes::from_static(&[0, 0, 20]),
            }]
            .into(),
            ..Default::default()
        };
        assert!(extensions(ingress).is_empty());
    }

    #[test]
    fn ingress_stream_identity_is_not_reused_on_egress() {
        let ingress = RtpExtensions {
            rid: Some(Rid::from("h")),
            opaque: vec![RtpExtension {
                uri: "urn:ietf:params:rtp-hdrext:sdes:mid".into(),
                value: Bytes::from_static(b"source"),
            }]
            .into(),
            ..Default::default()
        };
        assert!(extensions(ingress).is_empty());
    }

    #[test]
    fn first_accepted_original_anchors_receiver_roc_without_resetting_late_holes() {
        let mut sender = Sender {
            ssrc: 1.into(),
            pt: 111.into(),
            sequence_origin: None,
            highest_accepted: None,
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
