use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulsebeam_runtime::net;
use str0m::bwe::Bitrate;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded},
    rtp::RtpPacket,
};

use crate::entity;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::track::{self, TrackMeta, TrackReceiver, TrackSender};

/// Represents the reason a participant's session was terminated.
#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error")]
    RtcError(#[from] RtcError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Unsupported media direction (must be SendOnly or RecvOnly)")]
    InvalidMediaDirection,
}

/// Represents an asynchronous action the `ParticipantCore` requires the `ParticipantActor` to perform.
#[derive(Debug)]
pub enum CoreEvent {
    /// A new track has been created locally and should be published to the room.
    SpawnTrack(Arc<TrackMeta>),
}

pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    pub rtc: Rtc,
    pub batcher: Batcher,
    pub upstream_allocator: UpstreamAllocator,
    pub downstream_allocator: DownstreamAllocator,
    disconnect_reason: Option<DisconnectReason>,
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
            upstream_allocator: UpstreamAllocator::new(),
            downstream_allocator: DownstreamAllocator::new(),
            disconnect_reason: None,
            events: Vec::with_capacity(32),
        }
    }

    pub fn disconnect_reason(&self) -> Option<&DisconnectReason> {
        self.disconnect_reason.as_ref()
    }

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
        let now = tokio::time::Instant::now();
        self.upstream_allocator.poll(now);
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
        self.update_desired_bitrate();
    }

    pub fn remove_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_id in tracks.keys() {
            self.downstream_allocator.remove_track(track_id);
        }
        self.update_desired_bitrate();
    }

    pub fn poll_rtc(&mut self) -> Option<Duration> {
        if self.disconnect_reason.is_some() {
            return None;
        }

        self.upstream_allocator.poll(tokio::time::Instant::now());

        let key_requests = self.upstream_allocator.drain_keyframe_requests();
        if !key_requests.is_empty() {
            let mut api = self.rtc.direct_api();
            for key in key_requests {
                if let Some(stream) = api.stream_rx_by_mid(key.request.mid, key.request.rid) {
                    stream.request_keyframe(key.request.kind);
                    tracing::debug!(?key.request, "requested keyframe for upstream");
                } else {
                    tracing::warn!(?key.request, "stream not found for keyframe request");
                }
            }
        }

        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.saturating_duration_since(Instant::now()));
                }
                Ok(Output::Transmit(tx)) => {
                    self.batcher.push_back(tx.destination, &tx.contents);
                }
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(e) => {
                    self.disconnect(e.into());
                    return None;
                }
            }
        }

        None
    }

    pub fn add_published_track(&mut self, track: TrackSender) {
        self.upstream_allocator.add_published_track(track);
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

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            Event::KeyframeRequest(req) => self.downstream_allocator.handle_keyframe_request(req),
            Event::EgressBitrateEstimate(bwe) => {
                let Some(current) = self.downstream_allocator.handle_bwe(bwe) else {
                    return;
                };
                self.rtc.bwe().set_current_bitrate(Bitrate::bps(current));
                self.update_desired_bitrate();
            }
            e => {
                tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn update_desired_bitrate(&mut self) {
        let desired_bitrate = self.downstream_allocator.desired_bitrate();
        let desired_bitrate = Bitrate::bps(desired_bitrate + desired_bitrate / 4);
        self.rtc.bwe().set_desired_bitrate(desired_bitrate);
        tracing::debug!("desired_bitrate={desired_bitrate}");
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
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn handle_incoming_rtp(&mut self, rtp: RtpPacket) {
        let now = tokio::time::Instant::now();
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            return;
        };
        let (mid, rid) = (stream.mid(), stream.rid());
        self.upstream_allocator
            .handle_incoming_rtp(mid, rid.as_ref(), rtp, now);
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        tracing::info!(%reason, "Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.rtc.disconnect();
    }
}
