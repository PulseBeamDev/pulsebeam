use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulsebeam_runtime::net;
use str0m::media::Mid;
use str0m::{
    Event, Input, Output, Rtc,
    media::{Direction, MediaAdded},
    rtp::RtpPacket,
};

use crate::entity;
use crate::participant::{batcher::Batcher, downstream::DownstreamAllocator};
use crate::track::{self, TrackMeta, TrackReceiver, TrackSender};

/// Represents an asynchronous action the `ParticipantCore` requires the `ParticipantActor` to perform.
#[derive(Debug)]
pub enum CoreEvent {
    /// A new track has been created locally and should be published to the room.
    SpawnTrack(Arc<TrackMeta>),
    /// The participant's connection has been terminated.
    Disconnect,
}

pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    pub rtc: Rtc,
    pub batcher: Batcher,
    pub published_tracks: HashMap<Mid, TrackSender>,
    pub downstream_allocator: DownstreamAllocator,
    events: Vec<CoreEvent>,
}

impl ParticipantCore {
    pub fn new(
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
        batcher_capacity: usize,
    ) -> Self {
        Self {
            participant_id,
            rtc,
            batcher: Batcher::with_capacity(batcher_capacity),
            published_tracks: HashMap::new(),
            downstream_allocator: DownstreamAllocator::new(),
            events: Vec::with_capacity(32),
        }
    }

    /// Drains all pending events for the actor to handle.
    pub fn drain_events(&mut self) -> impl Iterator<Item = CoreEvent> + '_ {
        self.events.drain(..)
    }

    pub fn handle_udp_packet(&mut self, packet: net::RecvPacket) {
        if let Ok(contents) = (*packet.buf).try_into() {
            let recv = str0m::net::Receive {
                proto: str0m::net::Protocol::Udp,
                source: packet.src,
                destination: packet.dst,
                contents,
            };
            let _ = self.rtc.handle_input(Input::Receive(Instant::now(), recv));
        } else {
            tracing::warn!(src = %packet.src, "Dropping malformed UDP packet");
        }
    }

    pub fn handle_timeout(&mut self) {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
    }

    pub fn handle_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_handle in tracks.values() {
            if track_handle.meta.id.origin_participant != self.participant_id {
                self.downstream_allocator.add_track(track_handle.clone());
            }
        }
    }

    pub fn remove_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_id in tracks.keys() {
            self.downstream_allocator.remove_track(track_id);
        }
    }

    /// Polls the inner RTC engine, handling all synchronous events and queueing async ones.
    pub fn poll_rtc(&mut self) -> Option<Duration> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.saturating_duration_since(Instant::now()));
                }
                Ok(Output::Transmit(tx)) => {
                    self.batcher.push_back(tx.destination, &tx.contents);
                }
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(_) => {
                    self.events.push(CoreEvent::Disconnect);
                    return None;
                }
            }
        }
        None
    }

    pub fn add_published_track(&mut self, track: TrackSender) {
        self.published_tracks
            .insert(track.meta.id.origin_mid, track);
    }

    pub fn handle_forward_rtp(&mut self, track_meta: Arc<TrackMeta>, rtp: &RtpPacket) {
        if let Some(mid) = self.downstream_allocator.handle_rtp(&track_meta, rtp) {
            let Some(pt) = self
                .rtc
                .media(mid)
                .and_then(|m| m.remote_pts().first().copied())
            else {
                return;
            };
            let mut api = self.rtc.direct_api();
            if let Some(writer) = api.stream_tx_by_mid(mid, None) {
                let _ = writer.write_rtp(
                    pt,
                    rtp.seq_no,
                    rtp.header.timestamp,
                    rtp.timestamp,
                    rtp.header.marker,
                    rtp.header.ext_vals.clone(),
                    true,
                    rtp.payload.clone(),
                );
            }
        } else {
            tracing::warn!(track_id = %track_meta.id, ssrc = %rtp.header.ssrc, "Dropping RTP packet for inactive track");
        }
    }

    /// Handles synchronous events from the RTC engine.
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.events.push(CoreEvent::Disconnect);
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            Event::KeyframeRequest(req) => self.downstream_allocator.handle_keyframe_request(req),
            Event::EgressBitrateEstimate(bwe) => self.downstream_allocator.handle_bwe(bwe),
            e => {
                tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id =
                    Arc::new(entity::TrackId::new(self.participant_id.clone(), media.mid));
                let track_meta = Arc::new(track::TrackMeta {
                    id: track_id,
                    kind: media.kind,
                    simulcast_rids: media.simulcast.map(|s| s.recv),
                });
                self.events.push(CoreEvent::SpawnTrack(track_meta));
            }
            Direction::SendOnly => {
                self.downstream_allocator.add_slot(media.mid, media.kind);
            }
            _ => self.events.push(CoreEvent::Disconnect),
        }
    }

    fn handle_incoming_rtp(&mut self, mut rtp: RtpPacket) {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            return;
        };
        let (mid, rid) = (stream.mid(), stream.rid());

        if let Some(track) = self.published_tracks.get_mut(&mid) {
            rtp.header.ext_vals.rid = rid;
            track.send(rid.as_ref(), rtp);
        } else {
            tracing::warn!(ssrc = %rtp.header.ssrc, %mid, ?rid, "Dropping incoming RTP packet; no published track found");
        }
    }
}
