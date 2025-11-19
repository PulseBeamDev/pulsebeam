use std::collections::HashMap;
use std::sync::Arc;
use str0m::media::{MediaKind, Mid};
use tokio::time::Instant;

use pulsebeam_runtime::net;
use str0m::bwe::BweKind;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded},
};

use crate::entity;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::rtp::RtpPacket;
use crate::track::{self, TrackReceiver};

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error")]
    RtcError(#[from] RtcError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Unsupported media direction (must be SendOnly or RecvOnly)")]
    InvalidMediaDirection,
}

#[derive(Debug)]
pub enum CoreEvent {
    SpawnTrack(TrackReceiver),
}

pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    pub rtc: Rtc,
    pub batcher: Batcher,
    pub upstream: UpstreamAllocator,
    pub downstream: DownstreamAllocator,
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
            upstream: UpstreamAllocator::new(),
            downstream: DownstreamAllocator::new(),
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
            let _ = self
                .rtc
                .handle_input(Input::Receive(Instant::now().into(), recv));
        } else {
            tracing::warn!(src = %packet.src, "Dropping malformed UDP packet");
        }
    }

    pub fn handle_timeout(&mut self) {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now().into()));
    }

    pub fn handle_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_handle in tracks.values() {
            if track_handle.meta.origin_participant != self.participant_id {
                self.downstream.add_track(track_handle.clone());
            }
        }
        self.update_desired_bitrate();
    }

    pub fn remove_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_id in tracks.keys() {
            self.downstream.remove_track(track_id);
        }
        self.update_desired_bitrate();
    }

    pub fn poll_stats(&mut self, now: Instant) {
        self.update_desired_bitrate();
        self.upstream.poll_stats(now);
    }

    pub fn poll_rtc(&mut self) -> Option<Instant> {
        if self.disconnect_reason.is_some() {
            return None;
        }

        self.upstream.poll(&mut self.rtc, Instant::now());

        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.into());
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

    pub fn handle_forward_rtp(&mut self, mid: Mid, pkt: RtpPacket) {
        let pt = {
            let Some(media) = self.rtc.media(mid) else {
                return;
            };
            let Some(pt) = media.remote_pts().first() else {
                return;
            };
            *pt
        };

        let mut api = self.rtc.direct_api();
        let Some(writer) = api.stream_tx_by_mid(mid, None) else {
            tracing::warn!(%mid, ssrc = %pkt.raw_header.ssrc, "Dropping RTP for invalid stream mid");
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
            pkt.raw_header.marker,
            pkt.raw_header.ext_vals,
            true,
            pkt.payload,
        ) {
            tracing::warn!(%mid, ssrc = %pkt.raw_header.ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            Event::KeyframeRequest(req) => self.downstream.handle_keyframe_request(req),
            Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                let (current, desired) = self.downstream.update_bitrate(available);
                self.rtc.bwe().set_current_bitrate(current);
                self.rtc.bwe().set_desired_bitrate(desired);
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
            e => {
                tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn update_desired_bitrate(&mut self) {
        let (current, desired) = self.downstream.update_allocations();
        self.rtc.bwe().set_current_bitrate(current);
        self.rtc.bwe().set_desired_bitrate(desired);
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = Arc::new(entity::TrackId::new());
                let track_meta = Arc::new(track::TrackMeta {
                    id: track_id,
                    origin_participant: self.participant_id.clone(),
                    kind: media.kind,
                    simulcast_rids: media.simulcast.map(|s| s.recv),
                });
                let (tx, rx) = track::new(media.mid, track_meta, 64);
                self.upstream.add_published_track(media.mid, tx);
                self.events.push(CoreEvent::SpawnTrack(rx));
            }
            Direction::SendOnly => {
                self.downstream.add_slot(media.mid, media.kind);
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn handle_incoming_rtp(&mut self, rtp: str0m::rtp::RtpPacket) {
        tracing::trace!("tracing:rtp_event={}", rtp.seq_no);
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            return;
        };
        let (mid, rid) = (stream.mid(), stream.rid());

        let Some(media) = self.rtc.media(mid) else {
            return;
        };

        let rtp = match media.kind() {
            MediaKind::Audio => RtpPacket::from_str0m(rtp, crate::rtp::Codec::Opus),
            MediaKind::Video => RtpPacket::from_str0m(rtp, crate::rtp::Codec::H264),
        };
        self.upstream.handle_incoming_rtp(mid, rid.as_ref(), rtp);
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
