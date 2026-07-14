use super::signaling::Signaling;
use ahash::{HashMap, HashMapExt};
#[cfg(feature = "deep-metrics")]
use metrics::{counter, histogram};
use pulsebeam_runtime::net::{self, RecvPacketBatch, Transport};
use pulsebeam_runtime::rand::RngCore;
use std::collections::VecDeque;
use std::time::Duration;
use str0m::bwe::BweKind;
use str0m::format::Codec;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid};
use str0m::net::Protocol;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded, Pt},
};
use tokio::time::Instant;

use crate::entity::{self, TrackId, TrackKind};
use crate::id::ShardId;
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::participant::signaling;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::rtp::RtpPacket;
use crate::track::{
    self, DataTrackDirection, DataTrackIntent, DataTrackIntentError, KEYFRAME_DEBOUNCE, StreamId,
    StreamWriter, Track,
};

const RESERVED_DATA_CHANNEL_COUNT: u16 = 2;
const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct TrackAvailability {
    in_topology: bool,
}

impl TrackAvailability {
    fn unpublished() -> Self {
        Self { in_topology: false }
    }

    fn published() -> Self {
        Self { in_topology: true }
    }
}

pub struct TrackMapping {
    pub mid: Mid,
    pub track_id: TrackId,
    pub kind: MediaKind,
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
    #[error("Exceeded maximum upstream tracks: only 2 video and 2 audio allowed")]
    TooManyUpstreamTracks,
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
    pub udp_batcher: Batcher,
    pub tcp_batcher: Batcher,
    pub downstream: DownstreamAllocator,
    pending_ingress: VecDeque<RecvPacketBatch>,

    // Warm: touched per poll cycle
    pub upstream: UpstreamAllocator,
    pub participant_id: entity::ParticipantId,
    last_keyframe_request: HashMap<StreamId, Instant>,

    published_tracks: HashMap<TrackId, Track>,
    track_availability: HashMap<TrackId, TrackAvailability>,

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
        let signaling = Signaling::new();
        let udp_batcher = Batcher::with_capacity(udp_gso_size);
        let tcp_batcher = Batcher::with_capacity(tcp_gso_size);

        let mut p = Self {
            pending_ingress: VecDeque::new(),
            participant_id: cfg.participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
            upstream: UpstreamAllocator::new(),
            downstream: DownstreamAllocator::new(cfg.participant_id, cfg.manual_sub, rng),
            disconnect_reason: None,
            signaling,
            last_slow_poll: Instant::now(),
            last_keyframe_request: HashMap::new(),
            published_tracks: HashMap::new(),
            track_availability: HashMap::new(),
            room_id: cfg.room_id,
            shard_id,
        };

        p.on_tracks_published(&cfg.available_tracks);
        p
    }

    pub fn on_ingress(&mut self, batch: net::RecvPacketBatch) {
        self.pending_ingress.push_back(batch);
    }

    pub fn on_timeout(&mut self, now: Instant) {
        let _ = self.rtc.handle_input(Input::Timeout(now.into()));
    }

    #[inline]
    pub fn on_forward_rtp(&mut self, stream_id: &StreamId, pkt: &RtpPacket) {
        let mut writer = StreamWriter(&mut self.rtc);
        let promoted = self.downstream.on_forward_rtp(stream_id, pkt, &mut writer);
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
        let mut writer = StreamWriter(&mut self.rtc);
        self.downstream
            .on_forward_audio_rtp(slot_idx, pkt, &mut writer);
    }

    #[tracing::instrument(skip_all, fields(participant_id = %self.participant_id))]
    pub fn on_tracks_published(&mut self, tracks: &[Track]) {
        for track in tracks {
            if track.meta.origin == self.participant_id {
                continue;
            }

            tracing::info!(
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

    pub fn handle_keyframe_request(&mut self, key: KeyframeRequest) {
        let mut api = self.rtc.direct_api();
        if let Some(stream) = api.stream_rx_by_mid(key.mid, key.rid) {
            stream.request_keyframe(key.kind);
            tracing::debug!(?key, "requested keyframe for upstream");
        } else {
            tracing::warn!(?key, "stream not found for keyframe request");
        }
    }

    pub fn handle_remote_keyframe_request(
        &mut self,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    ) {
        let now = Instant::now();
        if let Some(last) = self.last_keyframe_request.get(&stream_id)
            && now.duration_since(*last) < KEYFRAME_DEBOUNCE
        {
            tracing::debug!(?stream_id, "debounced duplicate keyframe request");
            return;
        }

        let Some(mid) = self.upstream.mid_for_track_id(stream_id.0) else {
            tracing::warn!(track = ?stream_id.0, "unknown upstream track for keyframe request");
            return;
        };

        self.last_keyframe_request.insert(stream_id, now);
        self.handle_keyframe_request(KeyframeRequest {
            mid,
            rid: stream_id.1,
            kind,
        });
    }

    pub fn handle_tick(&mut self) {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now().into()));
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        let assignments_changed = self.downstream.poll_slow(now, &mut self.rtc.bwe(), events);
        if assignments_changed {
            self.signaling.mark_assignments_dirty();
        }
        self.upstream.poll_slow(now);
    }

    pub fn poll(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        'drain: loop {
            let Some(rtc_deadline) = self.poll_rtc(events) else {
                events.exit();
                return;
            };

            if now >= self.last_slow_poll + SLOW_POLL_INTERVAL {
                self.poll_slow(now, events);
                self.last_slow_poll = now;
                continue;
            }

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
                    tracing::warn!(src = %batch.src, "Dropping malformed UDP packet");
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
                continue 'drain;
            }

            let did_work = self.signaling.poll(&mut self.rtc, &self.downstream);
            if did_work {
                continue;
            }

            if self.downstream.dirty_allocation {
                let assignments_changed = self.downstream.update_allocations(&mut self.rtc.bwe());
                if assignments_changed {
                    self.signaling.mark_assignments_dirty();
                }
                self.downstream.reconcile_routes(now, events);
                continue;
            }

            let next_slow_poll = self.last_slow_poll + SLOW_POLL_INTERVAL;
            let deadline = rtc_deadline.min(next_slow_poll);

            if deadline > now {
                events.update_deadline(deadline);
                return;
            }

            let _ = self.rtc.handle_input(Input::Timeout(now.into()));
        }
    }

    /// Internal helper: Drains the RTC engine until it yields a Timeout.
    /// Handles Transmits (UDP/TCP) and Events (Logic).
    fn poll_rtc(&mut self, events: &mut impl ParticipantSink) -> Option<Instant> {
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
                        Protocol::Udp => self.udp_batcher.push_back(tx.destination, &tx.contents),
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
                    self.handle_event(event, events);
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

    fn handle_event(&mut self, e: Event, events: &mut impl ParticipantSink) {
        match e {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => self.handle_media_added(media, events),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp, events),
            Event::KeyframeRequest(req) => {
                if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                    events.request_keyframe(layer);
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                self.downstream.update_bitrate(available)
            }
            Event::ChannelOpen(cid, label) => {
                tracing::info!("{} is opened", label);
                let intent = match DataTrackIntent::try_from(label) {
                    Ok(intent) => intent,
                    Err(err) => {
                        self.disconnect(err.into());
                        return;
                    }
                };

                match intent {
                    DataTrackIntent::InternalSignaling => {
                        tracing::info!("internal media signaling is opened");
                        self.signaling.set_cid(cid);
                    }

                    DataTrackIntent::UserTopic { direction, topic } => {
                        let _topic = self.participant_id.derive_track_id(TrackKind::Data, &topic);
                        match direction {
                            DataTrackDirection::Publish => {
                                // self.upstream.add_data_track(cid, topic);
                            }
                            DataTrackDirection::Subscribe => {
                                // self.downstream.add_data_track(cid, topic);
                            }
                        }
                    }
                }
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
                events.publish_track(track.clone());
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

    #[tracing::instrument(skip_all, fields(participant_id = %self.participant_id, mid = %media.mid))]
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

        let Some(pt) = self.preferred_send_pt(mid, kind) else {
            tracing::warn!(%mid, ?kind, "no negotiated PT available for downstream slot");
            return;
        };

        let ssrc = {
            let mut api = self.rtc.direct_api();
            let Some(stream) = api.stream_tx_by_mid(mid, None) else {
                tracing::warn!(%mid, ?kind, "missing stream_tx_by_mid while adding downstream slot");
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
        tracing::trace!("tracing:rtp_event={}", rtp.seq_no);
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            return;
        };
        let (mid, rid) = (stream.mid(), stream.rid());

        let Some(media) = self.rtc.media(mid) else {
            return;
        };

        let (mut rtp, sr) = match media.kind() {
            MediaKind::Audio => RtpPacket::from_str0m(rtp, crate::rtp::Codec::Opus),
            MediaKind::Video => RtpPacket::from_str0m(rtp, crate::rtp::Codec::H264),
        };
        if self
            .upstream
            .handle_incoming_rtp(mid, rid.as_ref(), &mut rtp, sr)
        {
            let track_id = self
                .upstream
                .track_id_for_mid(mid)
                .expect("handle_incoming_rtp returned true so mid must have a slot");
            let stream_id: StreamId = (track_id, rid);
            events.publish_rtp(stream_id, rtp);
        }
    }

    #[tracing::instrument(skip(self), fields(participant_id = %self.participant_id, %reason))]
    pub fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        tracing::info!("Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.rtc.disconnect();
    }
}
