use super::signaling::Signaling;
use ahash::{HashMap, HashMapExt};
#[cfg(feature = "deep-metrics")]
use metrics::{counter, histogram};
use pulsebeam_proto::namespace;
use pulsebeam_runtime::net::{self, RecvPacketBatch, Transport};
use std::collections::VecDeque;
use std::time::Duration;
use str0m::bwe::BweKind;
use str0m::channel::ChannelConfig;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::Protocol;
use str0m::rtp::Ssrc;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded},
};
use tokio::time::Instant;

use crate::entity::{self, ParticipantId, ParticipantKey, TrackId};
use crate::participant::downstream::SlotConfig;
use crate::participant::signaling;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::rtp::RtpPacket;
use crate::shard::worker::Router;
use crate::track::{self, KEYFRAME_DEBOUNCE, StreamId, Track};

const RESERVED_DATA_CHANNEL_COUNT: u16 = 2;
const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub enum ParticipantEvent {
    PublishedTrack(Track),
    PublishedRtp(StreamId, RtpPacket),
    NewDeadline((Instant, ParticipantKey)),
    Exited(ParticipantKey),
    KeyframeRequest {
        origin: ParticipantId,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    },
}

pub type ParticipantEvents = VecDeque<ParticipantEvent>;

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
    #[error("Exceeded maximum upstream tracks: only 1 video and 1 audio allowed")]
    TooManyUpstreamTracks,
    #[error("Room closed")]
    RoomClosed,
    #[error("System terminated")]
    SystemTerminated,
}

#[derive(Debug)]
pub struct ParticipantConfig {
    pub manual_sub: bool,
    pub participant_id: entity::ParticipantId,
    pub rtc: Rtc,
    pub available_tracks: Vec<Track>,
}

pub struct ParticipantCore {
    // Hot: touched on every packet
    pub key: ParticipantKey,
    pub rtc: Rtc,
    pub udp_batcher: Batcher,
    pub tcp_batcher: Batcher,
    pub downstream: DownstreamAllocator,
    slot_meta: HashMap<Mid, (Pt, Ssrc)>,
    first_poll: bool,
    pending_ingress: VecDeque<RecvPacketBatch>,

    // Warm: touched per poll cycle
    pub upstream: UpstreamAllocator,
    pub participant_id: entity::ParticipantId,
    last_keyframe_request: HashMap<StreamId, Instant>,

    // Cold: touched rarely
    disconnect_reason: Option<DisconnectReason>,
    signaling: Signaling,
    last_slow_poll: Instant,
}

impl ParticipantCore {
    pub fn new(cfg: ParticipantConfig, udp_gso_size: usize, tcp_gso_size: usize) -> Self {
        let mut rtc = cfg.rtc;
        let mut api = rtc.direct_api();
        let cid = api.create_data_channel(ChannelConfig {
            label: namespace::Signaling::Reliable.as_str().to_string(),
            ordered: true,
            reliability: str0m::channel::Reliability::Reliable,
            negotiated: Some(0),
            protocol: "v1".to_string(),
        });
        // reserving sctp IDs for future expansion
        for i in 1..RESERVED_DATA_CHANNEL_COUNT {
            api.create_data_channel(ChannelConfig {
                label: "".to_string(),
                ordered: true,
                reliability: str0m::channel::Reliability::Reliable,
                negotiated: Some(i),
                protocol: "v1".to_string(),
            });
        }

        let udp_batcher = Batcher::with_capacity(udp_gso_size);
        let tcp_batcher = Batcher::with_capacity(tcp_gso_size);

        let mut p = Self {
            pending_ingress: VecDeque::new(),
            first_poll: true,
            key: 0,
            participant_id: cfg.participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
            upstream: UpstreamAllocator::new(),
            downstream: DownstreamAllocator::new(cfg.participant_id, cfg.manual_sub),
            slot_meta: HashMap::new(),
            disconnect_reason: None,
            signaling: Signaling::new(cid),
            last_slow_poll: Instant::now(),
            last_keyframe_request: HashMap::new(),
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
            && now.duration_since(*last) < KEYFRAME_DEBOUNCE {
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

    pub fn remove_available_tracks(&mut self, _tracks: &HashMap<entity::TrackId, Track>) {
        // for track in tracks.values() {
        //     self.downstream.remove_track(track);
        // }
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
    }

    #[tracing::instrument(skip_all, fields(participant_id = %self.participant_id))]
    fn poll_slow(&mut self, now: Instant, router: &mut Router, events: &mut ParticipantEvents) {
        let keyframe_requests = self
            .downstream
            .update_allocations(&mut self.rtc.bwe(), router);
        for req in keyframe_requests {
            let kind = req.kind;
            if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                events.push_back(ParticipantEvent::KeyframeRequest {
                    origin: layer.meta.origin,
                    stream_id: layer.stream_id(),
                    kind,
                });
            }
        }
        self.upstream.poll_slow(now);
    }

    pub fn poll(&mut self, now: Instant, events: &mut ParticipantEvents, router: &mut Router) {
        let next_deadline = self.poll_until_deadline(now, events, router);

        if let Some(next_deadline) = next_deadline {
            events.push_back(ParticipantEvent::NewDeadline((
                next_deadline,
                self.key,
            )));
        } else {
            events.push_back(ParticipantEvent::Exited(self.key));
        }
    }

    fn poll_until_deadline(
        &mut self,
        now: Instant,
        events: &mut ParticipantEvents,
        router: &mut Router,
    ) -> Option<Instant> {
        if now >= self.last_slow_poll + SLOW_POLL_INTERVAL {
            self.poll_slow(now, router, events);
            self.last_slow_poll = now;
        }

        while let Some(batch) = self.pending_ingress.pop_front() {
            let transport = match batch.transport {
                Transport::Udp(_) => str0m::net::Protocol::Udp,
                Transport::Tcp => str0m::net::Protocol::Tcp,
            };

            for pkt in batch.into_iter() {
                if let Ok(contents) = (*pkt).try_into() {
                    let recv = str0m::net::Receive {
                        proto: transport,
                        source: batch.src,
                        destination: batch.dst,
                        contents,
                    };
                    if self
                        .rtc
                        .handle_input(Input::Receive(now.into(), recv))
                        .is_err()
                    {
                        continue;
                    }

                    self.poll_rtc(events)?;
                } else {
                    tracing::warn!(src = %batch.src, "Dropping malformed UDP packet");
                }
            }
        }

        loop {
            let rtc_deadline = self.poll_rtc(events)?;
            let did_work = self.signaling.poll(&mut self.rtc, &self.downstream);
            if did_work {
                // Signaling wrote data. The RTC engine is now "dirty" (has output to send).
                // We loop back to `poll_rtc` immediately to flush `Output::Transmit`.
                continue;
            }

            if self.downstream.dirty_allocation {
                // Make sure rtc is updated with new allocations
                let keyframe_requests = self
                    .downstream
                    .update_allocations(&mut self.rtc.bwe(), router);
                for req in keyframe_requests {
                    let kind = req.kind;
                    if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                        events.push_back(ParticipantEvent::KeyframeRequest {
                            origin: layer.meta.origin,
                            stream_id: layer.stream_id(),
                            kind,
                        });
                    }
                }
                continue;
            }

            let next_slow_poll = self.last_slow_poll + SLOW_POLL_INTERVAL;

            // No new work generated. We are synced. Return the RTC deadline.
            return Some(rtc_deadline.min(next_slow_poll));
        }
    }

    /// Internal helper: Drains the RTC engine until it yields a Timeout.
    /// Handles Transmits (UDP/TCP) and Events (Logic).
    fn poll_rtc(&mut self, events: &mut ParticipantEvents) -> Option<Instant> {
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

    pub fn handle_forward_rtp(&mut self, mid: Mid, pkt: &RtpPacket) {
        let Some(&(pt, ssrc)) = self.slot_meta.get(&mid) else {
            tracing::warn!(%mid, "Dropping RTP: mid not in slot_meta (slot not yet negotiated)");
            return;
        };

        let mut api = self.rtc.direct_api();
        let Some(writer) = api.stream_tx(&ssrc) else {
            tracing::warn!(%mid, %ssrc, "Dropping RTP for invalid stream mid");
            return;
        };

        tracing::trace!(
            "forward rtp: seqno={},rtp_ts={:?},playout_time={:?}",
            pkt.seq_no,
            pkt.rtp_ts,
            pkt.playout_time
        );
        if let Err(err) = writer.write_rtp(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.marker,
            pkt.ext_vals.clone(),
            true,
            pkt.payload.to_vec(),
        ) {
            tracing::warn!(%mid, %ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }

    fn handle_event(&mut self, e: Event, events: &mut ParticipantEvents) {
        match e {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => self.handle_media_added(media, events),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp, events),
            Event::KeyframeRequest(req) => {
                let kind = req.kind;
                if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                    events.push_back(ParticipantEvent::KeyframeRequest {
                        origin: layer.meta.origin,
                        stream_id: layer.stream_id(),
                        kind,
                    });
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                self.downstream.update_bitrate(available)
            }
            Event::ChannelOpen(_cid, _label) => {}
            Event::ChannelData(data) => {
                if data.id == self.signaling.cid
                    && let Err(err) = self
                        .signaling
                        .handle_input(&data.data, &mut self.downstream)
                {
                    self.disconnect(err.into());
                }
            }
            // rtp monitor handles this
            Event::StreamPaused(_) => {
                //     if e.paused {
                //         return;
                //     }
                //
                //     let Some(track) = self.upstream_allocator.get_track_mut(&e.mid) else {
                //         return;
                //     };
                //     let Some(layer) = track.by_rid_mut(&e.rid) else {
                //         return;
                //     };
                //
                //     layer.monitor.set_manual_pause(e.paused);
            }
            _ => {
                // tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    #[tracing::instrument(skip_all, fields(participant_id = %self.participant_id, mid = %media.mid))]
    fn handle_media_added(&mut self, media: MediaAdded, events: &mut ParticipantEvents) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = self.participant_id.derive_track_id(media.kind, &media.mid);
                let track_meta = track::TrackMeta {
                    id: track_id,
                    origin: self.participant_id,
                    kind: media.kind,
                };
                match media.kind {
                    MediaKind::Audio => {
                        let (tx, track) = track::new_audio(media.mid, track_meta);
                        let accepted = self.upstream.add_published_track(media.mid, tx);
                        if !accepted {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                        events.push_back(ParticipantEvent::PublishedTrack(track));
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
                        }
                        events.push_back(ParticipantEvent::PublishedTrack(track));
                    }
                }
            }
            Direction::SendOnly => {
                self.signaling
                    .set_slot_count(self.downstream.video.slot_count());
                if let Some(m) = self.rtc.media(media.mid)
                    && let Some(&pt) = m.remote_pts().first()
                {
                    let mut api = self.rtc.direct_api();
                    if let Some(stream) = api.stream_tx_by_mid(media.mid, None) {
                        self.slot_meta.insert(media.mid, (pt, stream.ssrc()));

                        self.downstream.add_slot(SlotConfig {
                            mid: media.mid,
                            // TODO: don't ignore simulcast receivers
                            rid: None,
                            pt,
                            ssrc: stream.ssrc(),
                            kind: media.kind,
                        });
                    }
                }
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn handle_incoming_rtp(&mut self, rtp: str0m::rtp::RtpPacket, events: &mut ParticipantEvents) {
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
            events.push_back(ParticipantEvent::PublishedRtp(stream_id, rtp));
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
