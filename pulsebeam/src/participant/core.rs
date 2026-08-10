use super::signaling::Signaling;
use ahash::{HashMap, HashMapExt};
#[cfg(feature = "deep-metrics")]
use metrics::{counter, histogram};
use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, RelDelivery, rel_control};
use pulsebeam_runtime::net::{self, RecvPacketBatch, Transport};
use pulsebeam_runtime::rand::RngCore;
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

use crate::entity::{self, TrackId};
use crate::id::ShardId;
#[cfg(debug_assertions)]
use crate::log::plog_error;
use crate::log::{LogCtx, plog_debug, plog_info, plog_trace, plog_warn};
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::participant::reliable::ReliableChannels;
use crate::participant::signaling;
use crate::participant::{
    batcher::{Batcher, OwnedPacketQueue},
    downstream::DownstreamAllocator,
    upstream::{MAX_UPSTREAM_ENCODED_STREAMS, UpstreamAllocator},
};
use crate::rtp::RtpPacket;
use crate::track::{
    self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent, DataTrackIntentError,
    KEYFRAME_DEBOUNCE, MAX_DATA_TOPIC_CHANNELS, StreamId, StreamWrite, StreamWriter, Topic, Track,
};
use str0m::rtp::{RtpWrite, Ssrc};

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct TrackAvailability {
    in_topology: bool,
}

impl TrackAvailability {
    fn unpublished() -> Self {
        Self { in_topology: false }
    }
}

#[derive(Clone, Copy)]
struct IncomingRtpRoute {
    mid: Mid,
    rid: Option<Rid>,
    upstream_slot: usize,
    track_id: TrackId,
}

pub struct TrackMapping {
    pub mid: Mid,
    pub track_id: TrackId,
    pub kind: MediaKind,
}

/// Routing is not allowed to mutate an `Rtc`; it only queues work for the
/// participant's mutate-then-drain loop.
enum PendingFanout {
    Sctp {
        topic: Topic,
        origin: entity::ParticipantId,
        pkt: Vec<u8>,
    },
    ReliableSctp {
        topic: Topic,
        origin: entity::ParticipantId,
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

/// One str0m mutation. The poll loop applies one item and immediately returns
/// to `poll_rtc()` before applying another mutation.
enum PendingRtcMutation {
    Sctp {
        topic: Topic,
        origin: entity::ParticipantId,
        pkt: Vec<u8>,
    },
    ReliableSctp {
        topic: Topic,
        origin: entity::ParticipantId,
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
    pub available_tracks: Vec<Track>,
}

impl ParticipantConfig {
    // TODO: wrap rtc instead
    pub fn ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }
}

pub struct ParticipantCore {
    // Hot: touched on every packet
    pub rtc: Rtc,
    pub udp_packets: OwnedPacketQueue,
    pub tcp_batcher: Batcher,
    pub downstream: DownstreamAllocator,
    incoming_rtp_routes: HashMap<Ssrc, IncomingRtpRoute>,
    stream_writer: StreamWriter,
    pending_ingress: VecDeque<RecvPacketBatch>,
    pending_timeout: Option<Instant>,
    pending_fanout: VecDeque<PendingFanout>,
    pending_rtc_mutations: VecDeque<PendingRtcMutation>,
    rtc_deadline: Option<Instant>,
    rtc_needs_drain: bool,
    exited: bool,
    #[cfg(debug_assertions)]
    egress_guard: crate::rtp::egress_guard::EgressGuard,

    // Warm: touched per poll cycle
    pub upstream: UpstreamAllocator,
    pub participant_id: entity::ParticipantId,
    last_keyframe_request: HashMap<StreamId, Instant>,

    published_tracks: HashMap<TrackId, Track>,
    track_availability: HashMap<TrackId, TrackAvailability>,
    data_topic_channels: HashMap<ChannelId, DataTopicChannel>,
    data_pub_channels: HashMap<Topic, ChannelId>,
    data_sub_channels: HashMap<(Topic, Option<entity::ParticipantId>), ChannelId>,
    reliable_channels: ReliableChannels,

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
    pub room_id: entity::RoomId,
    pub shard_id: ShardId,
}

impl ParticipantCore {
    pub fn new(
        cfg: ParticipantConfig,
        shard_id: ShardId,
        udp_gso_size: usize,
        tcp_gso_size: usize,
        rng: &mut impl RngCore,
    ) -> Self {
        let rtc = cfg.rtc;
        let ctx = LogCtx {
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        };
        let signaling = Signaling::new(ctx);
        let udp_packets = OwnedPacketQueue::with_capacity(udp_gso_size);
        let tcp_batcher = Batcher::with_capacity(tcp_gso_size);

        let mut p = Self {
            pending_ingress: VecDeque::new(),
            pending_timeout: None,
            pending_fanout: VecDeque::new(),
            pending_rtc_mutations: VecDeque::new(),
            rtc_deadline: None,
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
            downstream: DownstreamAllocator::new(ctx, cfg.manual_sub, rng),
            incoming_rtp_routes: HashMap::with_capacity(MAX_UPSTREAM_ENCODED_STREAMS),
            disconnect_reason: None,
            signaling,
            last_slow_poll: Instant::now(),
            last_keyframe_request: HashMap::new(),
            published_tracks: HashMap::new(),
            track_availability: HashMap::new(),
            data_topic_channels: HashMap::new(),
            data_pub_channels: HashMap::new(),
            data_sub_channels: HashMap::new(),
            reliable_channels: ReliableChannels::new(),
            room_id: cfg.room_id,
            shard_id,
        };

        p.on_tracks_published(&cfg.available_tracks);
        p
    }

    pub(crate) fn log_ctx(&self) -> LogCtx {
        LogCtx {
            room_id: self.room_id,
            participant_id: self.participant_id,
        }
    }

    pub fn on_ingress(&mut self, batch: net::RecvPacketBatch) {
        self.pending_ingress.push_back(batch);
    }

    pub fn on_timeout(&mut self, now: Instant) {
        self.pending_timeout = Some(now);
    }

    #[inline]
    /// A track's latest measurements, pushed by the shard when they change.
    pub fn update_layer_states(&mut self, track_id: TrackId, states: &crate::track::TrackStates) {
        self.downstream.update_layer_states(track_id, states);
    }

    pub fn on_forward_rtp(
        &mut self,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
    ) {
        // Observation for the simulator: media payload actually forwarded to this subscriber,
        // to compare against what it received (i.e. how much of the link was video vs overhead).
        // Compiles out without the `sim` feature.
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_forwarded_media(
            &self.participant_id.to_string(),
            pkt.payload.len() as u64,
        );
        let promoted =
            self.downstream
                .on_forward_rtp(track_id, pkt, cache, &mut self.stream_writer);
        if promoted {
            self.signaling.mark_assignments_dirty();
        }
    }

    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        slot_idx: crate::id::AudioSelectorSlotId,
        pkt: &RtpPacket,
    ) {
        self.downstream
            .on_forward_audio_rtp(slot_idx, pkt, &mut self.stream_writer);
    }

    #[inline]
    pub fn on_forward_sctp(&mut self, topic: &Topic, origin: entity::ParticipantId, pkt: &[u8]) {
        self.pending_fanout.push_back(PendingFanout::Sctp {
            topic: topic.clone(),
            origin,
            pkt: pkt.to_vec(),
        });
    }

    pub fn on_forward_reliable_sctp(
        &mut self,
        topic: &Topic,
        origin: entity::ParticipantId,
        frame: &[u8],
    ) {
        self.pending_fanout.push_back(PendingFanout::ReliableSctp {
            topic: topic.clone(),
            origin,
            frame: frame.to_vec(),
        });
    }

    pub fn on_deliver_reliable_control(&mut self, topic: &Topic, bytes: &[u8]) {
        self.pending_fanout
            .push_back(PendingFanout::ReliableControl {
                topic: topic.clone(),
                bytes: bytes.to_vec(),
            });
    }

    pub fn on_tracks_published(&mut self, tracks: &[Track]) {
        for track in tracks {
            if track.meta.origin == self.participant_id {
                continue;
            }

            plog_info!(
                self.log_ctx(),
                track = %track.meta.id,
                origin = %track.meta.origin,
                "participant received published track"
            );
            self.downstream.add_track(track.clone());
        }
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
        self.signaling.reconcile(&mut self.downstream);
    }

    pub fn on_tracks_unpublished(&mut self, tracks: &[TrackId]) -> bool {
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

    pub fn ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }

    pub fn disconnect_reason(&self) -> Option<&DisconnectReason> {
        self.disconnect_reason.as_ref()
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

    pub fn handle_remote_keyframe_request(
        &mut self,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    ) {
        self.pending_fanout
            .push_back(PendingFanout::Keyframe { stream_id, kind });
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

    /// Hand the shard any measurement that has moved.
    ///
    /// On the fast path as well as the slow poll, because `process_packet`
    /// flips activity and health per packet and the allocator acts on those.
    fn publish_changed_stats(&mut self, events: &mut impl ParticipantSink) {
        for (track_id, states) in self.upstream.take_changed_stats() {
            events.publish_track_stats(track_id, states);
        }
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        // Measure before allocating: the monitors produce this tick's numbers,
        // and running the allocator first would decide against last tick's.
        self.upstream.poll_slow(now);
        self.publish_changed_stats(events);
        let assignments_changed = self.downstream.poll_slow(now, &mut self.rtc.bwe(), events);
        if assignments_changed {
            self.signaling.mark_assignments_dirty();
        }
    }

    /// Converts one routed item into zero or more deferred `Rtc` mutations.
    /// This only changes allocator state; actual str0m writes are performed by
    /// `apply_one_rtc_mutation` below.
    fn process_one_fanout(&mut self) -> bool {
        let Some(work) = self.pending_fanout.pop_front() else {
            return false;
        };

        match work {
            PendingFanout::Sctp { topic, origin, pkt } => {
                self.pending_rtc_mutations
                    .push_back(PendingRtcMutation::Sctp { topic, origin, pkt });
            }
            PendingFanout::ReliableSctp {
                topic,
                origin,
                frame,
            } => {
                self.pending_rtc_mutations
                    .push_back(PendingRtcMutation::ReliableSctp {
                        topic,
                        origin,
                        frame,
                    });
            }
            PendingFanout::ReliableControl { topic, bytes } => {
                self.pending_rtc_mutations
                    .push_back(PendingRtcMutation::ReliableControl { topic, bytes });
            }
            PendingFanout::Keyframe { stream_id, kind } => {
                self.pending_rtc_mutations
                    .push_back(PendingRtcMutation::Keyframe { stream_id, kind });
            }
        }

        true
    }

    /// Performs exactly one `Rtc` mutation. The caller must immediately resume
    /// the drain loop before this method can be called again.
    fn apply_one_rtc_mutation(&mut self, now: Instant) -> bool {
        if let Some(write) = self.stream_writer.pop() {
            self.apply_stream_write(write, now);
            return true;
        }

        let Some(mutation) = self.pending_rtc_mutations.pop_front() else {
            return false;
        };

        match mutation {
            PendingRtcMutation::Sctp { topic, origin, pkt } => {
                if let Some(cid) = self.data_sub_channels.get(&(topic.clone(), None)).copied() {
                    self.write_to_data_channel(cid, &topic, &pkt);
                }
                if let Some(cid) = self
                    .data_sub_channels
                    .get(&(topic.clone(), Some(origin)))
                    .copied()
                {
                    self.write_to_data_channel(cid, &topic, &pkt);
                }
            }
            PendingRtcMutation::ReliableSctp {
                topic,
                origin,
                frame,
            } => {
                if let Some(cid) = self.reliable_channels.subscriber_channel(&topic) {
                    let delivery = RelDelivery {
                        publisher_id: origin.as_str(),
                        frame,
                    };
                    self.write_to_data_channel(cid, &topic, &delivery.encode_to_vec());
                }
            }
            PendingRtcMutation::ReliableControl { topic, bytes } => {
                if let Some(cid) = self.reliable_channels.publisher_channel(&topic) {
                    self.write_to_data_channel(cid, &topic, &bytes);
                }
            }
            PendingRtcMutation::Keyframe { stream_id, kind } => {
                self.handle_remote_keyframe_request_now(stream_id, kind, now);
            }
        }

        true
    }

    fn write_to_data_channel(&mut self, cid: ChannelId, topic: &Topic, pkt: &[u8]) {
        let ctx = self.log_ctx();
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
            panic!("egress stream invariant violated: {violation}");
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
            pkt.rtp_ts.numer() as u32,
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

        let mut budget = 3usize;
        'drain: loop {
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
                let now = deadline.max(now);
                let _ = self.rtc.handle_input(Input::Timeout(now.into()));
                self.rtc_needs_drain = true;
                continue;
            }

            if self.apply_one_rtc_mutation(now) {
                self.rtc_needs_drain = true;
                continue;
            }

            self.publish_changed_stats(events);

            if now.saturating_duration_since(self.last_slow_poll) >= SLOW_POLL_INTERVAL {
                self.poll_slow(now, events);
                self.last_slow_poll = now;
                self.rtc_needs_drain = true;
                continue;
            }

            let ctx = self.log_ctx();
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
                let _ = self.rtc.handle_input(Input::Receive(now.into(), recv));
                self.rtc_needs_drain = true;
                continue 'drain;
            }

            if self.process_one_fanout() {
                continue;
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
            let deadline = self
                .rtc_deadline
                .expect("drained RTC must have a deadline")
                .min(next_slow_poll);

            // upper bounded to 3 ticks to defensively avoid spin loops from bugs or just to give fairness
            // to other participants
            if deadline <= now && budget > 0 {
                budget = budget.saturating_sub(1);
                let _ = self.rtc.handle_input(Input::Timeout(now.into()));
                self.rtc_needs_drain = true;
                continue;
            }

            return Some(deadline);
        }
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

        let result = loop {
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
                    events.request_keyframe(layer);
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
                        if let Some(previous) = self.data_topic_channels.remove(&cid) {
                            self.release_data_topic_channel(previous, events);
                        }

                        if self.data_topic_channels.len() >= MAX_DATA_TOPIC_CHANNELS {
                            self.disconnect(DisconnectReason::TooManyDataTopicChannels);
                            return;
                        }

                        if e.lane == DataLane::Reliable {
                            if self.reliable_channels.open(cid, &e, events).is_err() {
                                self.disconnect(DisconnectReason::DuplicateDataChannelLabel(e));
                                return;
                            }
                            self.data_topic_channels.insert(cid, e);
                            return;
                        }

                        let duplicate = match e.direction {
                            DataTrackDirection::Publish => self
                                .data_pub_channels
                                .get(&e.topic)
                                .copied()
                                .filter(|existing| *existing != cid),
                            DataTrackDirection::Subscribe => self
                                .data_sub_channels
                                .get(&(e.topic.clone(), e.scope))
                                .copied()
                                .filter(|existing| *existing != cid),
                        };
                        let conflicting_subscribe = e.direction == DataTrackDirection::Subscribe
                            && match e.scope {
                                Some(_) => self
                                    .data_sub_channels
                                    .contains_key(&(e.topic.clone(), None)),
                                None => self
                                    .data_sub_channels
                                    .keys()
                                    .any(|(topic, _)| *topic == e.topic),
                            };
                        if duplicate.is_some() || conflicting_subscribe {
                            self.disconnect(DisconnectReason::DuplicateDataChannelLabel(e));
                            return;
                        }

                        self.data_topic_channels.insert(cid, e.clone());
                        match e.direction {
                            DataTrackDirection::Publish => {
                                self.data_pub_channels.insert(e.topic.clone(), cid);
                                events.publish_data_topic(e.topic);
                            }
                            DataTrackDirection::Subscribe => {
                                self.data_sub_channels
                                    .insert((e.topic.clone(), e.scope), cid);
                                events.subscribe_data_topic(e.topic, e.scope);
                            }
                        }
                    }
                }
            }
            Event::ChannelClose(cid) => {
                let Some(ch) = self.data_topic_channels.remove(&cid) else {
                    return;
                };
                plog_info!(self.log_ctx(), "{} is closed", ch.topic);
                self.release_data_topic_channel(ch, events);
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

                if let Some(ch) = self.data_topic_channels.get(&data.id)
                    && data.binary
                {
                    match (ch.lane, ch.direction) {
                        (DataLane::Realtime, DataTrackDirection::Publish) => {
                            events.publish_sctp(ch.topic.clone(), data.data.to_vec());
                        }
                        (DataLane::Reliable, DataTrackDirection::Publish) => {
                            events.publish_reliable_sctp(ch.topic.clone(), data.data.to_vec());
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
        let Some(track_id) = self.upstream.track_id_for_mid(mid) else {
            return;
        };

        let state = self
            .track_availability
            .entry(track_id)
            .or_insert_with(TrackAvailability::unpublished);

        if active {
            if state.in_topology {
                return;
            }

            if let Some(track) = self.published_tracks.get(&track_id) {
                let states = self.upstream.layer_states_for(track_id);
                events.publish_track(track.clone(), states);
                state.in_topology = true;
            }
            return;
        }

        if !state.in_topology {
            return;
        }

        events.unpublish_track(track_id);
        state.in_topology = false;
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
                    shard_id: self.shard_id,
                    id: track_id,
                    origin: self.participant_id,
                };
                match media.kind {
                    MediaKind::Audio => {
                        let (tx, track) = track::new_audio(media.mid, track_meta);
                        let accepted = self.upstream.add_published_track(media.mid, tx);
                        if !accepted {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                            return;
                        }
                        self.published_tracks.insert(track.meta.id, track.clone());
                        self.track_availability
                            .insert(track.meta.id, TrackAvailability::unpublished());
                    }
                    MediaKind::Video => {
                        let (tx, track) = track::new_video(
                            media.mid,
                            track_meta,
                            media.simulcast.map(|s| s.recv).unwrap_or_default(),
                        );
                        let accepted = self.upstream.add_published_track(media.mid, tx);
                        if !accepted {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                            return;
                        }
                        self.published_tracks.insert(track.meta.id, track.clone());
                        self.track_availability
                            .insert(track.meta.id, TrackAvailability::unpublished());
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
        let route = if let Some(route) = self.incoming_rtp_routes.get(&ssrc).copied() {
            route
        } else {
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
                mid,
                rid,
                upstream_slot,
                track_id,
            };
            self.incoming_rtp_routes
                .retain(|_, cached| cached.mid != mid || cached.rid != rid);
            self.incoming_rtp_routes.insert(ssrc, route);
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
            let stream_id: StreamId = (route.track_id, route.rid);
            events.publish_rtp(stream_id, rtp);
        } else {
            self.incoming_rtp_routes.remove(&ssrc);
        }
    }

    fn cleanup_data_topics(&mut self, events: &mut impl ParticipantSink) {
        let channels: Vec<_> = self.data_topic_channels.drain().collect();

        for (cid, ch) in channels {
            let _ = cid;
            self.release_data_topic_channel(ch, events);
        }

        self.data_pub_channels.clear();
        self.data_sub_channels.clear();
        self.reliable_channels.clear();
    }

    fn release_data_topic_channel(
        &mut self,
        ch: DataTopicChannel,
        events: &mut impl ParticipantSink,
    ) {
        if ch.lane == DataLane::Reliable {
            self.reliable_channels.close(ch, events);
            return;
        }

        match ch.direction {
            DataTrackDirection::Publish => {
                self.data_pub_channels.remove(&ch.topic);
                events.unpublish_data_topic(ch.topic);
            }
            DataTrackDirection::Subscribe => {
                let removed = self.data_sub_channels.remove(&(ch.topic.clone(), ch.scope));
                debug_assert!(removed.is_some());
                events.unsubscribe_data_topic(ch.topic, ch.scope);
            }
        }
    }

    pub fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        plog_info!(self.log_ctx(), %reason, "Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.rtc.disconnect();
        self.rtc_needs_drain = true;
    }
}
