use super::signaling::Signaling;
use ahash::{HashMap, HashMapExt};
#[cfg(feature = "deep-metrics")]
use metrics::{counter, histogram};
use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, rel_control};
use pulsebeam_runtime::net::{self, RecvPacketBatch, Transport};
use slotmap::SecondaryMap;
use std::collections::VecDeque;
use std::time::Duration;
use str0m::bwe::BweKind;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use str0m::net::Protocol;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded, Pt},
};
use tokio::time::Instant;

use crate::entity::{self, TrackId, TrackKind};
use crate::id::ShardId;
use crate::keys::TrackKey;
#[cfg(debug_assertions)]
use crate::log::plog_error;
use crate::log::{LogCtx, plog_debug, plog_info, plog_trace, plog_warn};
use crate::participant::data::DataState;
use crate::participant::downstream::SlotConfig;
use crate::participant::effect::ParticipantEffect;
use crate::participant::event::ParticipantSink;
use crate::participant::signaling;
use crate::participant::{TrackPacket, TrackPacketRef};
use crate::participant::{
    batcher::{AppendStatus, Batcher, NetworkEgress, OwnedPacketQueue},
    downstream::DownstreamAllocator,
    upstream::{MAX_UPSTREAM_ENCODED_STREAMS, UpstreamAllocator},
};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use crate::track::{
    self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent, DataTrackIntentError,
    KEYFRAME_DEBOUNCE, MAX_DATA_TOPIC_CHANNELS, StreamId, StreamWrite, StreamWriter, Topic, Track,
};
use str0m::rtp::{RtpWrite, Ssrc};

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_WORK_BUDGET: usize = 256;
const RTC_OUTPUT_BUDGET: usize = 128;
const MAX_PENDING_INGRESS: usize = 256;
const MAX_PENDING_FANOUT: usize = 256;

fn inline_rtc_timeout(deadline: Instant, wall_now: Instant) -> Option<Instant> {
    let latest = wall_now
        .checked_add(pulsebeam_runtime::SHARD_TIMER_QUANTUM)
        .unwrap_or(wall_now);
    (deadline <= latest).then_some(deadline.max(wall_now))
}

#[derive(Clone, Copy)]
struct IncomingRtpRoute {
    ssrc: Ssrc,
    mid: Mid,
    rid: Option<Rid>,
    upstream_slot: usize,
    track_id: TrackId,
    /// The track's compiled fanout, resolved once when this route is cached
    /// rather than per packet. `None` until the controller installs the track
    /// image on this participant.
    fanout: Option<TrackKey>,
}

/// Upstream stream routing, stored as parallel arrays.
///
/// Every received RTP packet searches this by SSRC, and the search reads only
/// the key. Holding the keys apart from the payload means a participant with
/// `n` upstream streams costs `n/16` cache lines to search rather than
/// `n * size_of::<IncomingRtpRoute>() / 64` — and each stream added after that
/// costs four bytes of scan, not fifty-odd. That ratio is the point: the number
/// of upstream streams is expected to grow, and this stays cheap while it does.
///
/// Both arrays are dense and index-aligned: `ssrcs[i]` describes `routes[i]`.
/// Removal is a `swap_remove` on both, so there are no holes to skip and no
/// tombstones to test.
#[derive(Default)]
struct UpstreamRouteTable {
    ssrcs: Vec<Ssrc>,
    routes: Vec<IncomingRtpRoute>,
}

/// The packing claim above is only true while an SSRC is four bytes.
const _: () = assert!(std::mem::size_of::<Ssrc>() == 4);

impl UpstreamRouteTable {
    fn index_of(&self, ssrc: Ssrc) -> Option<usize> {
        self.ssrcs.iter().position(|&known| known == ssrc)
    }

    fn get(&self, ssrc: Ssrc) -> Option<IncomingRtpRoute> {
        self.routes.get(self.index_of(ssrc)?).copied()
    }

    fn insert(&mut self, route: IncomingRtpRoute) {
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
        if let Some(index) = self.index_of(route.ssrc) {
            if let Some(slot) = self.routes.get_mut(index) {
                *slot = route;
            }
            return;
        }
        if self.routes.len() >= MAX_UPSTREAM_ENCODED_STREAMS {
            debug_assert!(
                false,
                "more encoded streams than MAX_UPSTREAM_ENCODED_STREAMS allows"
            );
            metrics::counter!("upstream_route_table_full").increment(1);
            return;
        }
        self.ssrcs.push(route.ssrc);
        self.routes.push(route);
    }

    fn remove(&mut self, ssrc: Ssrc) {
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
        let Some(index) = self.index_of(ssrc) else {
            return;
        };
        self.ssrcs.swap_remove(index);
        self.routes.swap_remove(index);
    }

    fn clear(&mut self) {
        self.ssrcs.clear();
        self.routes.clear();
    }

    fn remove_track(&mut self, track_id: TrackId) {
        let mut index = 0;
        while index < self.routes.len() {
            let matches = self
                .routes
                .get(index)
                .is_some_and(|route| route.track_id == track_id);
            if matches {
                self.ssrcs.swap_remove(index);
                self.routes.swap_remove(index);
            } else {
                index = index.saturating_add(1);
            }
        }
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
    }

    /// Fill in the fanout for every stream of `track_id`.
    ///
    /// A method rather than an `iter_mut`, so the SSRC a route is filed under
    /// cannot be edited out from under the key array.
    fn bind_fanout(&mut self, track_id: TrackId, fanout: TrackKey) {
        for route in &mut self.routes {
            if route.track_id == track_id {
                route.fanout = Some(fanout);
            }
        }
    }
}

pub struct TrackMapping {
    pub mid: Mid,
    pub track_id: TrackId,
    pub kind: MediaKind,
}

struct TrackCatalogEntry {
    participant_id: entity::ParticipantId,
    track_id: TrackId,
}

type TrackCatalog = SecondaryMap<TrackKey, TrackCatalogEntry>;

/// Routing is not allowed to mutate an `Rtc`; it only queues work for the
/// participant's mutate-then-drain loop.
/// One deferred str0m mutation. The poll loop applies one item and immediately
/// returns to `poll_rtc()` before applying another mutation.
enum PendingRtcMutation {
    Sctp {
        channel: ChannelId,
        pkt: Vec<u8>,
    },
    ReliableSctp {
        channel: ChannelId,
        frame: Vec<u8>,
    },
    ReliableControl {
        topic: Topic,
        bytes: Vec<u8>,
    },
    Keyframe {
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error")]
    RtcError(#[from] RtcError),
    #[error("Signaling error")]
    SignalingError(#[from] signaling::SignalingError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Unsupported media direction (must be SendOnly or RecvOnly)")]
    InvalidMediaDirection,
    #[error("Invalid data channel protocol: {0}")]
    InvalidDataTrackIntent(#[from] DataTrackIntentError),
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error("Exceeded maximum upstream tracks: only 2 video and 2 audio allowed")]
    TooManyUpstreamTracks,
    #[error(
        "Exceeded maximum data topic channels: only 64 channels (across all topics/scopes) allowed"
    )]
    TooManyDataTopicChannels,
    #[error("Room closed")]
    RoomClosed,
    #[error("System terminated")]
    SystemTerminated,
}

#[derive(Debug)]
pub struct ParticipantConfig {
    pub manual_sub: bool,
    pub room_id: entity::RoomId,
    pub participant_id: entity::ParticipantId,
    pub rtc: Rtc,
}

pub enum ParticipantInput<'a> {
    Network {
        batch: net::RecvPacketBatch,
        source_shard: crate::id::ShardId,
    },
    Timeout(Instant),
    Track {
        key: TrackKey,
        packet: TrackPacketRef<'a>,
        cache: Option<&'a TrackStreamCache>,
    },
    ReliableControl {
        stream: TrackKey,
        bytes: &'a [u8],
    },
    Keyframe {
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    },
}

impl ParticipantConfig {
    // TODO: wrap rtc instead
    pub fn ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }
}

pub struct ParticipantCore {
    // Hot: touched on every packet
    rtc: Rtc,
    udp_packets: OwnedPacketQueue,
    tcp_batcher: Batcher,
    downstream: DownstreamAllocator,
    incoming_rtp_routes: UpstreamRouteTable,
    stream_writer: StreamWriter,
    pending_ingress: VecDeque<RecvPacketBatch>,
    pending_timeout: Option<Instant>,
    pending_mutations: VecDeque<PendingRtcMutation>,
    last_ingress: Option<(std::net::SocketAddr, std::net::SocketAddr)>,
    last_ingress_shard: Option<crate::id::ShardId>,
    rtc_deadline: Option<Instant>,
    rtc_clock: Instant,
    rtc_needs_drain: bool,
    exited: bool,
    #[cfg(debug_assertions)]
    egress_guard: crate::rtp::egress_guard::EgressGuard,

    // Warm: touched per poll cycle
    upstream: UpstreamAllocator,
    pub(crate) participant_id: entity::ParticipantId,
    last_keyframe_request: HashMap<StreamId, Instant>,

    /// The compiled stream a published channel forwards into, recorded by the
    /// shard once it has minted one. Keyed by channel so an arriving SCTP
    /// frame reaches its fanout without hashing a room, a publisher or a
    /// topic — the identity it would otherwise have to reassemble on every
    /// packet.
    track_keys: HashMap<TrackId, TrackKey>,
    catalog: TrackCatalog,
    data: DataState,

    /// Attributes str0m's own logs to this participant, for the simulator only.
    ///
    /// str0m is a library and logs without any notion of which peer it is serving, so a trace
    /// contains interleaved lines from every connection with nothing to tell them apart. That is
    /// not a cosmetic problem: probe results were read off such a trace and attributed to the
    /// wrong link, producing a confident and wrong diagnosis. A span makes the attribution part
    /// of the record instead of an inference.
    ///
    /// Built once here rather than per call. Constructing a span allocates and records fields,
    /// so doing it inside the poll loop is what makes this expensive; entering an existing one is
    /// comparatively cheap. It is still `sim`-only, because on the packet path even that cost is
    /// unwarranted for something only a human reading a trace benefits from.
    #[cfg(feature = "sim")]
    sim_span: tracing::Span,

    // Cold: touched rarely
    disconnect_reason: Option<DisconnectReason>,
    signaling: Signaling,
    last_slow_poll: Instant,
    pub(crate) room_id: entity::RoomId,
    pub(crate) shard_id: ShardId,
}

impl ParticipantCore {
    /// Record the fanout a published track forwards into.
    ///
    /// Patches the per-SSRC route cache as well as the index: a route cached
    /// before the shard minted the fanout would otherwise keep reporting
    /// `None` for the life of the stream, and the miss would never heal.
    fn track_fanout(&self, track_id: TrackId) -> Option<TrackKey> {
        self.track_keys.get(&track_id).copied()
    }

    /// Record the stream a published data topic forwards into.
    ///
    /// Called by the shard once it has minted the arena entry, which happens
    /// a step after the participant announced the topic. Until it lands, a
    /// frame on that channel falls back to a room-scoped lookup; afterwards
    /// the key rides on the event and nothing on the packet path hashes a
    /// name.
    fn bind_published_data_stream(&mut self, topic: &Topic, stream: TrackKey) {
        if let Some(&channel) = self.data.published_channels.get(topic) {
            self.data.published_streams.insert(channel, stream);
        } else {
            self.data
                .pending_published_streams
                .insert(topic.clone(), stream);
        }
    }

    fn bind_published_reliable_stream(&mut self, topic: &Topic, stream: TrackKey) {
        self.data
            .reliable_stream_topics
            .insert(stream, topic.clone());
        if let Some(channel) = self.data.reliable.publisher_channel(topic) {
            self.data.reliable_published_streams.insert(channel, stream);
        } else {
            self.data
                .pending_reliable_streams
                .insert(topic.clone(), stream);
        }
    }

    pub fn new(
        cfg: ParticipantConfig,
        shard_id: ShardId,
        udp_gso_size: usize,
        tcp_gso_size: usize,
    ) -> Self {
        let rtc = cfg.rtc;
        let ctx = LogCtx {
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        };
        let signaling = Signaling::new(ctx);
        let udp_packets = OwnedPacketQueue::with_capacity(udp_gso_size);
        let tcp_batcher = Batcher::with_capacity(tcp_gso_size);

        let now = Instant::now();
        Self {
            pending_ingress: VecDeque::new(),
            pending_timeout: None,
            pending_mutations: VecDeque::new(),
            last_ingress: None,
            last_ingress_shard: None,
            rtc_deadline: None,
            rtc_clock: now,
            rtc_needs_drain: true,
            exited: false,
            #[cfg(debug_assertions)]
            egress_guard: crate::rtp::egress_guard::EgressGuard::new(),
            #[cfg(feature = "sim")]
            sim_span: tracing::info_span!(
                "peer",
                participant_id = %cfg.participant_id,
                room_id = %cfg.room_id
            ),
            stream_writer: StreamWriter::new(),
            participant_id: cfg.participant_id,
            rtc,
            udp_packets,
            tcp_batcher,
            upstream: UpstreamAllocator::new(ctx),
            downstream: DownstreamAllocator::new(ctx, cfg.manual_sub),
            incoming_rtp_routes: UpstreamRouteTable::default(),
            disconnect_reason: None,
            signaling,
            last_slow_poll: now,
            last_keyframe_request: HashMap::new(),
            track_keys: HashMap::new(),
            catalog: SecondaryMap::new(),
            data: DataState::new(),
            room_id: cfg.room_id,
            shard_id,
        }
    }

    fn log_ctx(&self) -> LogCtx {
        LogCtx {
            room_id: self.room_id,
            participant_id: self.participant_id,
        }
    }

    pub fn apply(&mut self, effect: ParticipantEffect) {
        match effect {
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                self.signaling.apply_participants(added, removed);
            }
            ParticipantEffect::TrackInstalled { key, track } => self.install_track(key, track),
            ParticipantEffect::TrackSourceBound { key, track_id } => {
                self.track_keys.insert(track_id, key);
                self.incoming_rtp_routes.bind_fanout(track_id, key);
            }
            ParticipantEffect::TrackSourceUnbound { key, track_id } => {
                if self.track_keys.get(&track_id) == Some(&key) {
                    let _ = self.track_keys.remove(&track_id);
                    self.incoming_rtp_routes.remove_track(track_id);
                }
            }
            ParticipantEffect::TrackRemoved(key) => self.remove_compiled_track(key),
            ParticipantEffect::TrackPublished { topic, key, lane } => match lane {
                DataLane::Realtime => self.bind_published_data_stream(&topic, key),
                DataLane::Reliable => self.bind_published_reliable_stream(&topic, key),
            },
            ParticipantEffect::TrackSubscribed { key, channel, lane } => {
                self.data.forwarding.insert(
                    key,
                    crate::participant::data::DataForwarding { lane, channel },
                );
            }
        }
    }

    fn install_track(&mut self, key: TrackKey, track: Track) {
        let track_id = track.id();
        let participant_id = track.meta().origin;
        debug_assert_eq!(track_id.kind(), track.kind());
        if let Some(previous) = self.catalog.get(key) {
            debug_assert_eq!(previous.track_id, track_id);
            debug_assert_eq!(previous.participant_id, participant_id);
            if participant_id != self.participant_id && track_id.kind() != TrackKind::Data {
                self.signaling.mark_assignments_dirty();
            }
            return;
        }
        let previous = self.catalog.insert(
            key,
            TrackCatalogEntry {
                participant_id,
                track_id,
            },
        );
        debug_assert!(previous.is_none(), "a TrackKey must be installed once");
        let previous = self.track_keys.insert(track_id, key);
        debug_assert!(previous.is_none() || previous == Some(key));
        if participant_id == self.participant_id {
            self.incoming_rtp_routes.bind_fanout(track_id, key);
        } else if track_id.kind() != TrackKind::Data {
            self.on_track_published(key, track);
        }
    }

    fn remove_compiled_track(&mut self, key: TrackKey) {
        let Some(binding) = self.catalog.remove(key) else {
            return;
        };
        self.data.forwarding.remove(key);
        if self.track_keys.get(&binding.track_id) == Some(&key) {
            let _ = self.track_keys.remove(&binding.track_id);
        }
        if binding.participant_id == self.participant_id {
            self.incoming_rtp_routes.remove_track(binding.track_id);
        } else if binding.track_id.kind() != TrackKind::Data {
            let _ = self.on_tracks_unpublished(std::slice::from_ref(&binding.track_id));
        }
    }

    pub fn input<'a>(&mut self, input: ParticipantInput<'a>) {
        match input {
            ParticipantInput::Network {
                batch,
                source_shard,
            } => self.on_ingress(batch, source_shard),
            ParticipantInput::Timeout(now) => self.on_timeout(now),
            ParticipantInput::Track { key, packet, cache } => {
                self.on_track_packet(key, packet, cache);
            }
            ParticipantInput::ReliableControl { stream, bytes } => {
                let Some(topic) = self.data.reliable_stream_topics.get(stream) else {
                    debug_assert!(false, "a reliable control stream must have a topic");
                    return;
                };
                self.enqueue_fanout(PendingRtcMutation::ReliableControl {
                    topic: topic.clone(),
                    bytes: bytes.to_vec(),
                });
            }
            ParticipantInput::Keyframe { stream_id, kind } => {
                self.enqueue_fanout(PendingRtcMutation::Keyframe { stream_id, kind });
            }
        }
    }

    fn on_track_packet(
        &mut self,
        key: TrackKey,
        packet: TrackPacketRef<'_>,
        cache: Option<&TrackStreamCache>,
    ) {
        let TrackPacketRef::Rtp(packet) = packet else {
            let (stream, lane, bytes) = match packet {
                TrackPacketRef::Data { lane, bytes } => (key, lane, bytes),
                TrackPacketRef::Rtp(_) => {
                    debug_assert!(false, "the RTP path must be handled before data dispatch");
                    return;
                }
            };
            let Some(binding) = self.data.forwarding.get(stream).copied() else {
                debug_assert!(false, "a data forwarding plan must have a receiver binding");
                return;
            };
            debug_assert_eq!(binding.lane, lane);
            let channel = binding.channel;
            match lane {
                crate::track::DataLane::Realtime => {
                    self.enqueue_fanout(PendingRtcMutation::Sctp {
                        channel,
                        pkt: bytes.to_vec(),
                    });
                }
                crate::track::DataLane::Reliable => {
                    self.enqueue_fanout(PendingRtcMutation::ReliableSctp {
                        channel,
                        frame: bytes.to_vec(),
                    });
                }
            }
            return;
        };
        let Some(entry) = self.catalog.get(key) else {
            debug_assert!(false, "a TrackPacket must target an installed track");
            return;
        };
        if entry.participant_id == self.participant_id {
            debug_assert!(false, "a forwarding plan must target a remote track");
            return;
        }
        if entry.participant_id == self.participant_id {
            debug_assert!(false, "a participant must never receive its own track");
            return;
        }
        let kind = entry.track_id.kind();
        let participant_id = entry.participant_id;
        let track_id = entry.track_id;
        match kind {
            TrackKind::Video => {
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_forwarded_media_for(
                    self.participant_id,
                    packet.payload.len() as u64,
                );
                let promoted = self.downstream.on_forward_rtp(
                    key,
                    packet.arrival_ts,
                    cache,
                    &mut self.stream_writer,
                );
                if promoted {
                    self.signaling.mark_assignments_dirty();
                }
            }
            TrackKind::Audio => {
                debug_assert!(cache.is_none(), "audio forwarding has no video cache");
                let origin = crate::entity::AudioOrigin {
                    participant: participant_id,
                    track: track_id,
                };
                self.downstream
                    .on_forward_audio_rtp(origin, packet, &mut self.stream_writer);
                if self.downstream.take_audio_speakers_changed() {
                    self.signaling.mark_assignments_dirty();
                }
            }
            TrackKind::Data => debug_assert!(false, "data tracks carry bytes, not RTP"),
        }
    }

    fn on_ingress(&mut self, batch: net::RecvPacketBatch, source_shard: crate::id::ShardId) {
        self.last_ingress = Some((batch.src, batch.dst));
        self.last_ingress_shard = Some(source_shard);
        if self.pending_ingress.len() >= MAX_PENDING_INGRESS {
            let _ = self.pending_ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.pending_ingress.push_back(batch);
    }

    pub fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                AppendStatus::Drained => break,
                AppendStatus::Full if !egress.flush() => break,
                AppendStatus::Full => {}
            }
        }
        loop {
            match egress.append_tcp(&mut self.tcp_batcher) {
                AppendStatus::Drained => break,
                AppendStatus::Full if !egress.flush() => break,
                AppendStatus::Full => {}
            }
        }
    }

    fn on_timeout(&mut self, now: Instant) {
        self.pending_timeout = Some(now);
    }

    fn on_track_published(&mut self, key: TrackKey, track: Track) {
        if track.meta().origin == self.participant_id {
            debug_assert!(false, "the controller must not install a loopback track");
            return;
        }
        plog_info!(
            self.log_ctx(),
            track = %track.id(),
            origin = %track.meta().origin,
            "participant received published track"
        );
        self.downstream.install_track(key, track);
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
        self.signaling.reconcile(&mut self.downstream);
    }

    fn on_tracks_unpublished(&mut self, tracks: &[TrackId]) -> bool {
        let mut removed = false;
        for track_id in tracks {
            removed |= self.downstream.remove_track(track_id);
        }
        if removed {
            self.signaling.mark_tracks_dirty();
            self.signaling.mark_assignments_dirty();
            self.signaling.reconcile(&mut self.downstream);
        }
        removed
    }

    fn handle_keyframe_request_now(&mut self, key: KeyframeRequest) {
        let ctx = self.log_ctx();
        let mut api = self.rtc.direct_api();
        if let Some(stream) = api.stream_rx_by_mid(key.mid, key.rid) {
            stream.request_keyframe(key.kind);
            plog_debug!(ctx, ?key, "requested keyframe for upstream");
        } else {
            plog_warn!(ctx, ?key, "stream not found for keyframe request");
        }
    }

    fn enqueue_fanout(&mut self, work: PendingRtcMutation) {
        if self.pending_mutations.len() >= MAX_PENDING_FANOUT {
            metrics::counter!("participant_fanout_shed").increment(1);
            return;
        }
        self.pending_mutations.push_back(work);
    }

    fn handle_remote_keyframe_request_now(
        &mut self,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
        now: Instant,
    ) {
        if let Some(last) = self.last_keyframe_request.get(&stream_id)
            && now.duration_since(*last) < KEYFRAME_DEBOUNCE
        {
            plog_debug!(
                self.log_ctx(),
                ?stream_id,
                "debounced duplicate keyframe request"
            );
            return;
        }

        let Some(mid) = self.upstream.mid_for_track_id(stream_id.0) else {
            plog_warn!(self.log_ctx(), track = ?stream_id.0, "unknown upstream track for keyframe request");
            return;
        };

        self.last_keyframe_request.insert(stream_id, now);
        self.handle_keyframe_request_now(KeyframeRequest {
            mid,
            rid: stream_id.1,
            kind,
        });
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        // Measure before allocating: the monitors produce this tick's numbers,
        // and running the allocator first would decide against last tick's.
        self.upstream.poll_slow(now);
        let assignments_changed = self.downstream.poll_slow(now, &mut self.rtc.bwe(), events);
        if assignments_changed {
            self.signaling.mark_assignments_dirty();
        }
    }

    /// Performs exactly one `Rtc` mutation. The caller must immediately resume
    /// the drain loop before this method can be called again.
    fn apply_one_rtc_mutation(&mut self, now: Instant) -> bool {
        if let Some(write) = self.stream_writer.pop() {
            self.apply_stream_write(write, now);
            return true;
        }

        let Some(mutation) = self.pending_mutations.pop_front() else {
            return false;
        };

        match mutation {
            PendingRtcMutation::Sctp { channel, pkt } => {
                self.write_to_data_channel(channel, &pkt);
            }
            PendingRtcMutation::ReliableSctp { channel, frame } => {
                self.write_to_data_channel(channel, &frame);
            }
            PendingRtcMutation::ReliableControl { topic, bytes } => {
                if let Some(cid) = self.data.reliable.publisher_channel(&topic) {
                    self.write_to_data_channel(cid, &bytes);
                }
            }
            PendingRtcMutation::Keyframe { stream_id, kind } => {
                self.handle_remote_keyframe_request_now(stream_id, kind, now);
            }
        }

        true
    }

    fn write_to_data_channel(&mut self, cid: ChannelId, pkt: &[u8]) {
        let ctx = self.log_ctx();
        let topic = self
            .data
            .topic_channels
            .get(&cid)
            .map(|channel| channel.topic.clone());
        let Some(mut ch) = self.rtc.channel(cid) else {
            return;
        };
        if let Err(err) = ch.write(true, pkt) {
            plog_warn!(
                ctx,
                ?topic,
                ?cid,
                ?err,
                "failed to forward data topic packet"
            );
        }
    }

    fn apply_stream_write(&mut self, write: StreamWrite, now: Instant) {
        let (pkt, mid, rid, ssrc, pt, kind) = match write {
            StreamWrite::Video {
                pkt,
                mid,
                rid,
                ssrc,
                pt,
            } => (pkt, mid, rid, ssrc, pt, MediaKind::Video),
            StreamWrite::Audio { pkt, mid, ssrc, pt } => {
                (pkt, mid, None, ssrc, pt, MediaKind::Audio)
            }
        };
        let nackable = kind == MediaKind::Video;

        let ctx = self.log_ctx();
        let mut api = self.rtc.direct_api();
        let (stream, recovered) = match api.stream_tx(&ssrc) {
            Some(stream) if stream.mid() == mid && stream.rid() == rid => (Some(stream), false),
            Some(stream) => {
                debug_assert!(stream.mid() != mid || stream.rid() != rid);
                (api.stream_tx_by_mid(mid, rid), true)
            }
            None => (api.stream_tx_by_mid(mid, rid), true),
        };
        let Some(stream) = stream else {
            if nackable {
                plog_warn!(ctx, target: crate::log::TARGET_VIDEO, %mid, ?rid, "no stream_tx_by_mid found");
            } else {
                plog_warn!(ctx, target: crate::log::TARGET_AUDIO, %mid, "no stream_tx_by_mid found");
            }
            return;
        };
        debug_assert_eq!(stream.mid(), mid);
        debug_assert_eq!(stream.rid(), rid);
        let ssrc = stream.ssrc();
        if recovered {
            let refreshed = self.downstream.refresh_ssrc(kind, mid, rid, ssrc);
            debug_assert!(refreshed, "recovered stream has no downstream slot");
        }
        #[cfg(debug_assertions)]
        if let Some(violation) =
            self.egress_guard
                .check(mid, rid, *pkt.seq_no, pkt.rtp_ts.numer(), pkt.marker, kind)
        {
            plog_error!(ctx, %mid, ?rid, %violation, "egress stream invariant violated");
            // Hard failure in simulation, where this is a bug to be found. A dev
            // build carrying real media keeps serving: one malformed stream is
            // not worth taking the node down for.
            #[cfg(feature = "sim")]
            pulsebeam_runtime::fatal!("egress stream invariant violated: {violation}");
        }
        if nackable {
            plog_trace!(
                ctx,
                target: crate::log::TARGET_VIDEO,
                %mid, ?rid, %ssrc, %pt, seq = %pkt.seq_no, len = pkt.payload.len(), marker = pkt.marker,
                "Writing RTP packet"
            );
        } else {
            plog_trace!(
                ctx,
                target: crate::log::TARGET_AUDIO,
                %mid, %ssrc, %pt, seq = %pkt.seq_no, len = pkt.payload.len(), marker = pkt.marker,
                "Writing RTP packet"
            );
        }
        // The sender's Video Layers Allocation describes its simulcast layers,
        // which is meaningless on the single stream we forward to the viewer.
        let mut ext_vals = pkt.ext_vals;
        ext_vals
            .user_values
            .remove::<str0m::rtp::vla::VideoLayersAllocation>();
        if let Some((min, max)) = self.downstream.playout_delay_to_stamp() {
            ext_vals.play_delay_min = Some(min);
            ext_vals.play_delay_max = Some(max);
            self.downstream
                .record_playout_delay_stamp(mid, rid, pkt.seq_no);
        }
        // str0m derives Sender Report and TWCC timing from this instant, so it
        // must be when the packet is handed over — not when it arrived. A switch
        // replays cached packets whose arrival is already in the past.
        let rtp = RtpWrite::new(
            pt,
            pkt.seq_no,
            u32::try_from(pkt.rtp_ts.numer() & u64::from(u32::MAX)).unwrap_or(0),
            now.into(),
            pkt.payload,
        )
        .nackable(nackable)
        .marker(pkt.marker)
        .ext_vals(ext_vals);
        stream.write_rtp(rtp);
    }

    pub fn poll(&mut self, now: Instant, events: &mut impl ParticipantSink) -> Option<Instant> {
        // A disconnect can be observed while work for this participant is still
        // queued, so poll can be re-entered after it has already exited.
        if self.exited {
            return None;
        }

        // Entered once per poll cycle rather than per packet, so every str0m line produced by
        // this participant's work carries its identity for the cost of one guard.
        //
        // Cloned first because the guard would otherwise hold a borrow of `self` for the whole
        // function. `Span` is a handle, so this is a refcount bump rather than a rebuild.
        #[cfg(feature = "sim")]
        let sim_span = self.sim_span.clone();
        #[cfg(feature = "sim")]
        let _sim_guard = sim_span.enter();

        self.advance_rtc_clock(now, now);
        let mut work_budget = POLL_WORK_BUDGET;
        'drain: loop {
            if work_budget == 0 {
                metrics::counter!("participant_poll_budget_hit").increment(1);
                let next = now.checked_add(Duration::from_micros(1)).unwrap_or(now);
                return Some(next);
            }
            work_budget = work_budget.saturating_sub(1);
            if self.rtc_needs_drain {
                let Some(rtc_deadline) = self.poll_rtc(now, events) else {
                    self.rtc_deadline = None;
                    self.rtc_needs_drain = false;
                    self.exited = true;
                    self.cleanup_data_topics(events);
                    events.exit();
                    return None;
                };
                self.rtc_deadline = Some(rtc_deadline);
                self.rtc_needs_drain = false;
            }
            debug_assert!(self.rtc_deadline.is_some());

            if let Some(deadline) = self.pending_timeout.take() {
                self.advance_rtc_clock(deadline.max(now), now);
                let _ = self.rtc.handle_input(Input::Timeout(self.rtc_clock.into()));
                self.rtc_needs_drain = true;
                continue;
            }

            if self.apply_one_rtc_mutation(now) {
                self.rtc_needs_drain = true;
                continue;
            }

            if now.saturating_duration_since(self.last_slow_poll) >= SLOW_POLL_INTERVAL {
                self.poll_slow(now, events);
                self.last_slow_poll = now;
                self.rtc_needs_drain = true;
                continue;
            }

            let ctx = self.log_ctx();
            self.advance_rtc_clock(now, now);
            let receive_at = self.rtc_clock;
            while let Some(batch) = self.pending_ingress.front_mut() {
                let transport = match batch.transport {
                    Transport::Udp(_) => str0m::net::Protocol::Udp,
                    Transport::Tcp => str0m::net::Protocol::Tcp,
                };

                let src = batch.src;
                let dst = batch.dst;
                let Some(pkt) = batch.next_packet() else {
                    self.pending_ingress.pop_front();
                    continue;
                };

                let Ok(contents) = (*pkt).try_into() else {
                    plog_warn!(ctx, src = %batch.src, "Dropping malformed UDP packet");
                    // no point iterating the batch, this is already malicous
                    self.pending_ingress.pop_front();
                    continue;
                };

                let recv = str0m::net::Receive {
                    proto: transport,
                    source: src,
                    destination: dst,
                    contents,
                };
                let _ = self
                    .rtc
                    .handle_input(Input::Receive(receive_at.into(), recv));
                self.rtc_needs_drain = true;
                continue 'drain;
            }

            let did_work = self.signaling.poll(&mut self.rtc, &self.downstream);
            if did_work {
                self.rtc_needs_drain = true;
                continue;
            }

            if self.downstream.dirty_allocation {
                let assignments_changed =
                    self.downstream.update_allocations(now, &mut self.rtc.bwe());
                if assignments_changed {
                    self.signaling.mark_assignments_dirty();
                }
                self.downstream.reconcile_routes(now, events);
                self.rtc_needs_drain = true;
                continue;
            }

            let next_slow_poll = self
                .last_slow_poll
                .checked_add(SLOW_POLL_INTERVAL)
                .unwrap_or(self.last_slow_poll);
            // A drained Rtc always reports one; falling back to the slow poll
            // costs a tick of latency rather than the process.
            let deadline = self
                .rtc_deadline
                .unwrap_or(next_slow_poll)
                .min(next_slow_poll);

            if let Some(rtc_now) = inline_rtc_timeout(deadline, now) {
                self.advance_rtc_clock(rtc_now, now);
                let _ = self.rtc.handle_input(Input::Timeout(self.rtc_clock.into()));
                self.rtc_needs_drain = true;
                continue;
            }

            return Some(deadline);
        }
    }

    fn advance_rtc_clock(&mut self, candidate: Instant, wall_now: Instant) {
        let previous = self.rtc_clock;
        self.rtc_clock = self.rtc_clock.max(candidate).max(wall_now);
        debug_assert!(
            self.rtc_clock >= previous,
            "participant RTC clock moved backwards"
        );
        debug_assert!(
            self.rtc_clock
                <= wall_now
                    .checked_add(pulsebeam_runtime::SHARD_TIMER_QUANTUM)
                    .unwrap_or(wall_now),
            "participant RTC clock advanced beyond one wheel quantum"
        );
    }

    /// Internal helper: Drains the RTC engine until it yields a Timeout.
    /// Handles Transmits (UDP/TCP) and Events (Logic).
    fn poll_rtc(&mut self, now: Instant, events: &mut impl ParticipantSink) -> Option<Instant> {
        // Count of useful outputs (Transmit / Event) processed in this call.
        #[cfg(feature = "deep-metrics")]
        let mut work_items: u64 = 0;
        #[cfg(feature = "deep-metrics")]
        let mut timeouts = 0;
        #[cfg(feature = "deep-metrics")]
        let mut transmits = 0;
        #[cfg(feature = "deep-metrics")]
        let mut event_count = 0;
        #[cfg(feature = "deep-metrics")]
        let mut errors = 0;

        let mut outputs = 0usize;
        let result = loop {
            if outputs >= RTC_OUTPUT_BUDGET {
                metrics::counter!("participant_rtc_output_budget_hit").increment(1);
                break Some(now);
            }
            if !self.rtc.is_alive() {
                break None;
            }
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    #[cfg(feature = "deep-metrics")]
                    {
                        timeouts += 1;
                    }
                    break Some(deadline.into());
                }
                Ok(Output::Transmit(tx)) => {
                    outputs = outputs.saturating_add(1);
                    #[cfg(feature = "deep-metrics")]
                    {
                        transmits += 1;
                        work_items += 1;
                    }
                    match tx.proto {
                        Protocol::Udp => self
                            .udp_packets
                            .push_back(tx.destination, tx.contents.into()),
                        Protocol::Tcp => self.tcp_batcher.push_back(tx.destination, &tx.contents),
                        _ => {}
                    }
                }
                Ok(Output::Event(event)) => {
                    outputs = outputs.saturating_add(1);
                    #[cfg(feature = "deep-metrics")]
                    {
                        event_count += 1;
                        work_items += 1;
                    }
                    self.handle_event(now, event, events);
                }
                Err(e) => {
                    #[cfg(feature = "deep-metrics")]
                    {
                        errors += 1;
                    }
                    self.disconnect(e.into());
                    break None;
                }
            }
        };

        #[cfg(feature = "deep-metrics")]
        {
            // Record how many useful outputs were processed per poll_rtc invocation.
            // A value of 0 means the first poll_output was already a Timeout (idle call).
            histogram!("poll_rtc_work_items_per_call").record(work_items as f64);
            counter!("poll_rtc_outputs_total", "kind" => "timeout").increment(timeouts);
            counter!("poll_rtc_outputs_total", "kind" => "transmit").increment(transmits);
            counter!("poll_rtc_outputs_total", "kind" => "event").increment(event_count);
            counter!("poll_rtc_outputs_total", "kind" => "error").increment(errors);
        }

        result
    }

    fn handle_event(&mut self, now: Instant, e: Event, events: &mut impl ParticipantSink) {
        match e {
            // `Connected` is DTLS; ICE reaching connected is what tells the
            // shard the peer address is authenticated, and that is handled
            // below. Falls through to the catch-all with every other event this
            // participant does not act on.
            Event::IceConnectionStateChange(state) if state.is_connected() => {
                if let (Some((source, destination)), Some(source_shard)) =
                    (self.last_ingress, self.last_ingress_shard)
                {
                    events.connected(source, destination, source_shard);
                }
            }
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => {
                self.incoming_rtp_routes.clear();
                self.handle_media_added(media, events);
            }
            Event::MediaChanged(_) => self.incoming_rtp_routes.clear(),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp, events),
            Event::KeyframeRequest(req) => {
                if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                    let stream_id = layer.stream_id();
                    let layer = layer.clone();
                    events.request_keyframe(&layer, self.track_fanout(stream_id.0));
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                self.downstream.update_bitrate(now, available);
            }
            Event::MediaEgressStats(stats) => {
                if let Some(remote) = stats.remote {
                    self.downstream.handle_egress_stats(
                        stats.mid,
                        stats.rid,
                        remote.maximum_sequence_number,
                    );
                }
            }
            Event::ChannelOpen(cid, _label) => {
                let Some(ch) = self.rtc.channel(cid) else {
                    return;
                };
                let Some(cfg) = ch.config() else {
                    return;
                };

                let intent = match DataTrackIntent::try_from(cfg) {
                    Ok(intent) => intent,
                    Err(err) => {
                        self.disconnect(err.into());
                        return;
                    }
                };

                match intent {
                    DataTrackIntent::InternalSignaling => {
                        plog_info!(self.log_ctx(), "internal media signaling is opened");
                        self.signaling.set_cid(cid);
                    }

                    DataTrackIntent::UserTopic(e) => {
                        plog_info!(self.log_ctx(), "{} is opened", e);
                        if let Some(previous) = self.data.topic_channels.remove(&cid) {
                            self.release_data_topic_channel(cid, previous, events);
                        }

                        if self.data.topic_channels.len() >= MAX_DATA_TOPIC_CHANNELS {
                            self.disconnect(DisconnectReason::TooManyDataTopicChannels);
                            return;
                        }

                        if e.lane == DataLane::Reliable {
                            if self.data.reliable.open(cid, &e, events).is_err() {
                                self.disconnect(DisconnectReason::DuplicateDataChannelLabel(e));
                                return;
                            }
                            if e.direction == DataTrackDirection::Publish
                                && let Some(stream) =
                                    self.data.pending_reliable_streams.remove(&e.topic)
                            {
                                self.data.reliable_published_streams.insert(cid, stream);
                            }
                            self.data.topic_channels.insert(cid, e);
                            return;
                        }

                        let duplicate = match e.direction {
                            DataTrackDirection::Publish => self
                                .data
                                .published_channels
                                .get(&e.topic)
                                .copied()
                                .filter(|existing| *existing != cid),
                            DataTrackDirection::Subscribe => self
                                .data
                                .subscribed_channels
                                .get(&(e.topic.clone(), e.scope))
                                .copied()
                                .filter(|existing| *existing != cid),
                        };
                        let conflicting_subscribe = e.direction == DataTrackDirection::Subscribe
                            && match e.scope {
                                Some(_) => self
                                    .data
                                    .subscribed_channels
                                    .contains_key(&(e.topic.clone(), None)),
                                None => self
                                    .data
                                    .subscribed_channels
                                    .keys()
                                    .any(|(topic, _)| *topic == e.topic),
                            };
                        if duplicate.is_some() || conflicting_subscribe {
                            self.disconnect(DisconnectReason::DuplicateDataChannelLabel(e));
                            return;
                        }

                        self.data.topic_channels.insert(cid, e.clone());
                        match e.direction {
                            DataTrackDirection::Publish => {
                                self.data.published_channels.insert(e.topic.clone(), cid);
                                if let Some(stream) =
                                    self.data.pending_published_streams.remove(&e.topic)
                                {
                                    self.data.published_streams.insert(cid, stream);
                                }
                                events
                                    .publish_data_topic(e.topic, crate::track::DataLane::Realtime);
                            }
                            DataTrackDirection::Subscribe => {
                                self.data
                                    .subscribed_channels
                                    .insert((e.topic.clone(), e.scope), cid);
                                events.subscribe_data_topic(
                                    e.topic,
                                    e.scope,
                                    cid,
                                    crate::track::DataLane::Realtime,
                                );
                            }
                        }
                    }
                }
            }
            Event::ChannelClose(cid) => {
                let Some(ch) = self.data.topic_channels.remove(&cid) else {
                    return;
                };
                plog_info!(self.log_ctx(), "{} is closed", ch.topic);
                self.release_data_topic_channel(cid, ch, events);
            }
            Event::ChannelData(data) => {
                if Some(data.id) == self.signaling.cid
                    && let Err(err) = self
                        .signaling
                        .handle_input(&data.data, &mut self.downstream)
                        .map(|input_events| {
                            for input_event in input_events {
                                self.handle_signaling_input(input_event, events);
                            }
                        })
                {
                    self.disconnect(err.into());
                    return;
                }

                if let Some(ch) = self.data.topic_channels.get(&data.id)
                    && data.binary
                {
                    match (ch.lane, ch.direction) {
                        (DataLane::Realtime, DataTrackDirection::Publish) => {
                            events.publish_sctp(
                                ch.topic.clone(),
                                self.data.published_streams.get(&data.id).copied(),
                                data.data.to_vec(),
                            );
                        }
                        (DataLane::Reliable, DataTrackDirection::Publish) => {
                            events.publish_reliable_sctp(
                                ch.topic.clone(),
                                self.data.reliable_published_streams.get(&data.id).copied(),
                                data.data.to_vec(),
                            );
                        }
                        (DataLane::Reliable, DataTrackDirection::Subscribe) => {
                            debug_assert!(ch.scope.is_none());
                            let Ok(control) = RelControl::decode(data.data.as_ref()) else {
                                return;
                            };
                            let Some(rel_control::Msg::Nack(nack)) = control.msg else {
                                return;
                            };
                            let Ok(publisher) = entity::ParticipantId::try_from(nack.publisher_id)
                            else {
                                return;
                            };
                            events.forward_reliable_control(
                                publisher,
                                ch.topic.clone(),
                                self.data.reliable_subscribed_streams.get(&data.id).copied(),
                                data.data.to_vec(),
                            );
                        }
                        (DataLane::Realtime, DataTrackDirection::Subscribe) => {}
                    }
                }
            }
            Event::StreamPaused(stream) => {
                self.handle_stream_paused(stream.mid, stream.paused, events);
            }
            _ => {
                // tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn handle_signaling_input(
        &mut self,
        event: signaling::SignalingInputEvent,
        events: &mut impl ParticipantSink,
    ) {
        match event {
            signaling::SignalingInputEvent::UpstreamTrackState { mid, active } => {
                self.handle_upstream_track_state(mid, active, events);
            }
        }
    }

    fn handle_upstream_track_state(
        &mut self,
        mid: Mid,
        active: bool,
        events: &mut impl ParticipantSink,
    ) {
        if active {
            let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid) else {
                return;
            };
            if *in_topology {
                return;
            }
            let track = descriptor.clone();
            *in_topology = true;
            events.publish_track(track);
            return;
        }

        let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid) else {
            return;
        };
        if !*in_topology {
            return;
        }
        let track_id = descriptor.id();
        *in_topology = false;
        events.unpublish_track(track_id);
    }

    fn handle_stream_paused(&mut self, mid: Mid, paused: bool, events: &mut impl ParticipantSink) {
        // Treat unpaused as an implicit publish signal from str0m.
        // We intentionally do not unpublish on paused=true here; explicit
        // client intent is authoritative for stop/unpublish transitions.
        if !paused {
            self.handle_upstream_track_state(mid, true, events);
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded, _events: &mut impl ParticipantSink) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = self
                    .participant_id
                    .derive_track_id(media.kind.into(), &media.mid);
                let track_meta = track::TrackMeta {
                    room_id: self.room_id,
                    shard_id: self.shard_id,
                    id: track_id,
                    origin: self.participant_id,
                };
                match media.kind {
                    MediaKind::Audio => {
                        let (tx, track) = track::new_audio(media.mid, track_meta);
                        if !self.upstream.add_published_track(media.mid, tx, track) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                    MediaKind::Video => {
                        let (tx, track) = track::new_video(
                            media.mid,
                            track_meta,
                            media.simulcast.map(|s| s.recv).unwrap_or_default(),
                        );
                        if !self.upstream.add_published_track(media.mid, tx, track) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                }
            }
            Direction::SendOnly => {
                self.try_add_downstream_slot(media.mid, media.kind);
                // Update signaling slot count AFTER adding the slot so the
                // server accepts ClientIntent requests up to the actual slot
                // count (previously this was called before add_slot, so the
                // count was always one behind and every intent was rejected).
                self.signaling
                    .set_slot_count(self.downstream.video.slot_count());
                self.signaling
                    .set_audio_slot_count(self.downstream.audio_slot_count());
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn preferred_send_pt(&self, mid: Mid, kind: MediaKind) -> Option<Pt> {
        let media = self.rtc.media(mid)?;
        let remote_pts = media.remote_pts();
        if remote_pts.is_empty() {
            return None;
        }

        let expected_codec = match kind {
            MediaKind::Audio => Codec::Opus,
            MediaKind::Video => Codec::H264,
        };

        let codec_config = self.rtc.codec_config();
        remote_pts
            .iter()
            .copied()
            .find(|pt| {
                codec_config
                    .params()
                    .iter()
                    .any(|params| params.pt() == *pt && params.spec().codec == expected_codec)
            })
            .or_else(|| {
                if kind.is_video() {
                    remote_pts.first().copied()
                } else {
                    None
                }
            })
    }

    fn try_add_downstream_slot(&mut self, mid: Mid, kind: MediaKind) {
        if self.downstream.has_slot(kind, mid) {
            return;
        }

        let ctx = self.log_ctx();
        let Some(pt) = self.preferred_send_pt(mid, kind) else {
            plog_warn!(ctx, %mid, ?kind, "no negotiated PT available for downstream slot");
            return;
        };

        let ssrc = {
            let mut api = self.rtc.direct_api();
            let Some(stream) = api.stream_tx_by_mid(mid, None) else {
                plog_warn!(ctx, %mid, ?kind, "missing stream_tx_by_mid while adding downstream slot");
                return;
            };
            stream.ssrc()
        };

        self.downstream.add_slot(SlotConfig {
            mid,
            // TODO: don't ignore simulcast receivers
            rid: None,
            pt,
            ssrc,
            kind,
        });
    }

    fn handle_incoming_rtp(
        &mut self,
        rtp: str0m::rtp::RtpPacket,
        events: &mut impl ParticipantSink,
    ) {
        plog_trace!(self.log_ctx(), "tracing:rtp_event={}", rtp.seq_no);
        let ssrc = rtp.header.ssrc;
        let route = if let Some(route) = self.incoming_rtp_routes.get(ssrc) {
            route
        } else {
            // Once per stream in a healthy participant. A rate proportional to
            // the packet rate means the table is evicting live routes.
            metrics::counter!("upstream_route_miss").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("upstream_route_miss");
            let mut api = self.rtc.direct_api();
            let Some(stream) = api.stream_rx(&ssrc) else {
                return;
            };
            let mid = stream.mid();
            let rid = stream.rid();
            let Some((upstream_slot, track_id)) = self.upstream.slot_for_mid(mid) else {
                return;
            };
            let route = IncomingRtpRoute {
                ssrc,
                mid,
                rid,
                upstream_slot,
                track_id,
                fanout: self.track_keys.get(&track_id).copied(),
            };
            self.incoming_rtp_routes.insert(route);
            route
        };

        let Some(media) = self.rtc.media(route.mid) else {
            return;
        };

        let (mut rtp, sr) = match media.kind() {
            MediaKind::Audio => RtpPacket::from_str0m(rtp, crate::rtp::Codec::Opus),
            MediaKind::Video => RtpPacket::from_str0m(rtp, crate::rtp::Codec::H264),
        };
        if self.upstream.handle_incoming_rtp(
            route.upstream_slot,
            route.mid,
            route.rid.as_ref(),
            &mut rtp,
            sr,
        ) {
            let packet = match media.kind() {
                MediaKind::Audio | MediaKind::Video => TrackPacket::Rtp(rtp),
            };
            events.publish_track_packet(route.fanout, packet);
        } else {
            self.incoming_rtp_routes.remove(ssrc);
        }
    }

    fn cleanup_data_topics(&mut self, events: &mut impl ParticipantSink) {
        let channels: Vec<_> = self.data.topic_channels.drain().collect();

        for (cid, ch) in channels {
            self.release_data_topic_channel(cid, ch, events);
        }

        self.data.published_channels.clear();
        self.data.subscribed_channels.clear();
        self.data.reliable.clear();
        self.data.pending_published_streams.clear();
        self.data.pending_reliable_streams.clear();
        self.data.reliable_published_streams.clear();
        self.data.reliable_subscribed_streams.clear();
    }

    fn release_data_topic_channel(
        &mut self,
        cid: ChannelId,
        ch: DataTopicChannel,
        events: &mut impl ParticipantSink,
    ) {
        if ch.lane == DataLane::Reliable {
            self.data.reliable_published_streams.remove(&cid);
            self.data.reliable_subscribed_streams.remove(&cid);
            if ch.direction == DataTrackDirection::Publish {
                self.data.pending_reliable_streams.remove(&ch.topic);
            }
            self.data.reliable.close(ch, events);
            return;
        }

        match ch.direction {
            DataTrackDirection::Publish => {
                self.data.published_streams.remove(&cid);
                self.data.pending_published_streams.remove(&ch.topic);
                self.data.published_channels.remove(&ch.topic);
                events.unpublish_data_topic(ch.topic, crate::track::DataLane::Realtime);
            }
            DataTrackDirection::Subscribe => {
                let removed = self
                    .data
                    .subscribed_channels
                    .remove(&(ch.topic.clone(), ch.scope));
                debug_assert!(removed.is_some());
                events.unsubscribe_data_topic(
                    ch.topic,
                    ch.scope,
                    cid,
                    crate::track::DataLane::Realtime,
                );
            }
        }
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        plog_info!(self.log_ctx(), %reason, "Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.rtc.disconnect();
        self.rtc_needs_drain = true;
    }
}

#[cfg(test)]
mod rtc_clock_tests {
    use super::*;

    #[test]
    fn sub_quantum_deadlines_do_not_accumulate_clock_lag() {
        let start = Instant::now();
        let mut wall = start;
        let mut rtc = start;

        for _ in 0..1_000 {
            wall += pulsebeam_runtime::SHARD_TIMER_QUANTUM;
            let deadline = rtc + Duration::from_micros(30);
            let candidate = inline_rtc_timeout(deadline, wall).expect("deadline is inline");
            rtc = rtc.max(candidate).max(wall);
            assert!(rtc >= wall);
            assert!(rtc <= wall + pulsebeam_runtime::SHARD_TIMER_QUANTUM);
        }

        assert_eq!(rtc, wall);
    }
}

#[cfg(test)]
mod upstream_route_table_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};

    fn track_id(label: &str) -> TrackId {
        entity::ParticipantId::from_bytes([7u8; 16])
            .derive_track_id(entity::TrackKind::Video, label)
    }

    fn route(ssrc: u32) -> IncomingRtpRoute {
        IncomingRtpRoute {
            ssrc: Ssrc::from(ssrc),
            mid: Mid::from("0"),
            rid: None,
            upstream_slot: 0,
            track_id: track_id("t"),
            fanout: None,
        }
    }

    /// The table used to be direct-mapped on `ssrc % MAX_UPSTREAM_ENCODED_STREAMS`,
    /// so any two SSRCs congruent modulo that constant evicted each other and
    /// every packet from both took the expensive `direct_api()` miss path. A
    /// client picks its SSRCs at random, so this is the common case, not a
    /// corner: among six streams the collision probability is over 90%.
    #[test]
    fn ssrcs_congruent_modulo_the_capacity_do_not_evict_each_other() {
        let mut table = UpstreamRouteTable::default();
        let ssrcs: Vec<u32> = (0..MAX_UPSTREAM_ENCODED_STREAMS)
            .map(|k| {
                0x1000_0000
                    + u32::try_from(k).unwrap()
                        * u32::try_from(MAX_UPSTREAM_ENCODED_STREAMS).unwrap()
            })
            .collect();

        for ssrc in &ssrcs {
            table.insert(route(*ssrc));
        }

        for ssrc in &ssrcs {
            assert_eq!(
                table.get(Ssrc::from(*ssrc)).map(|route| route.ssrc),
                Some(Ssrc::from(*ssrc)),
                "every inserted route must remain retrievable"
            );
        }
    }

    /// The key array and the payload array must describe the same streams.
    ///
    /// They are separate allocations kept aligned by index, which is what makes
    /// the search read four bytes per stream instead of the whole route. A
    /// removal that reorders one and not the other silently starts routing a
    /// stream's packets to another stream's track — no panic, no metric, just
    /// media arriving on the wrong slot. `swap_remove` on both is what keeps
    /// them aligned, and reordering is exactly what it does.
    #[test]
    fn removal_keeps_the_key_and_payload_arrays_describing_the_same_streams() {
        let mut table = UpstreamRouteTable::default();
        let ssrcs: Vec<u32> = (0..MAX_UPSTREAM_ENCODED_STREAMS)
            .map(|k| 0x2000_0000 + u32::try_from(k).unwrap())
            .collect();
        for ssrc in &ssrcs {
            table.insert(route(*ssrc));
        }

        // Remove from the front, which is where swap_remove reorders most.
        for removed in 0..ssrcs.len() {
            table.remove(Ssrc::from(ssrcs[removed]));
            assert_eq!(
                table.ssrcs.len(),
                table.routes.len(),
                "the arrays must stay the same length"
            );
            for (index, key) in table.ssrcs.iter().enumerate() {
                assert_eq!(
                    table.routes[index].ssrc, *key,
                    "routes[{index}] must be the route for ssrcs[{index}]"
                );
            }
            for surviving in ssrcs.iter().skip(removed.saturating_add(1)) {
                assert_eq!(
                    table.get(Ssrc::from(*surviving)).map(|r| r.ssrc),
                    Some(Ssrc::from(*surviving)),
                    "removing one stream must not lose another"
                );
            }
        }
        assert!(table.ssrcs.is_empty() && table.routes.is_empty());
    }

    /// The property that outlives any particular table implementation: a route
    /// that fits is a route that resolves, whatever the SSRCs happen to be.
    #[test]
    fn every_route_that_fits_is_retrievable() {
        let mut rng = seeded_rng(0xB0A7);
        for _ in 0..256 {
            let mut table = UpstreamRouteTable::default();
            let mut ssrcs = Vec::with_capacity(MAX_UPSTREAM_ENCODED_STREAMS);
            while ssrcs.len() < MAX_UPSTREAM_ENCODED_STREAMS {
                let candidate = rng.next_u32();
                if !ssrcs.contains(&candidate) {
                    ssrcs.push(candidate);
                }
            }

            for ssrc in &ssrcs {
                table.insert(route(*ssrc));
            }
            for ssrc in &ssrcs {
                assert!(
                    table.get(Ssrc::from(*ssrc)).is_some(),
                    "ssrc {ssrc:#x} was evicted by a route that should have had its own slot"
                );
            }
        }
    }

    #[test]
    fn reinserting_an_ssrc_updates_in_place_rather_than_consuming_a_slot() {
        let mut table = UpstreamRouteTable::default();
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            table.insert(route(1000 + u32::try_from(k).unwrap()));
        }
        let mut updated = route(1000);
        updated.upstream_slot = 5;
        table.insert(updated);

        assert_eq!(
            table.get(Ssrc::from(1000u32)).map(|r| r.upstream_slot),
            Some(5)
        );
        for k in 1..MAX_UPSTREAM_ENCODED_STREAMS {
            assert!(
                table
                    .get(Ssrc::from(1000 + u32::try_from(k).unwrap()))
                    .is_some()
            );
        }
    }

    #[test]
    fn removing_one_route_leaves_the_others() {
        let mut table = UpstreamRouteTable::default();
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            table.insert(route(2000 + u32::try_from(k).unwrap()));
        }
        table.remove(Ssrc::from(2003u32));

        assert!(table.get(Ssrc::from(2003u32)).is_none());
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            if k != 3 {
                assert!(
                    table
                        .get(Ssrc::from(2000 + u32::try_from(k).unwrap()))
                        .is_some()
                );
            }
        }
    }
}
