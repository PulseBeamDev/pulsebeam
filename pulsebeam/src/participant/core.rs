use super::signaling::Signaling;
use pulsebeam_proto::namespace;
use pulsebeam_runtime::net::{self, Transport};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use str0m::bwe::BweKind;
use str0m::channel::ChannelConfig;
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use str0m::net::Protocol;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded},
};
use tokio::time::Instant;

use crate::entity::{self, TrackId};
use crate::participant::signaling;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::rtp::RtpPacket;
use crate::track::{self, TrackReceiver};

const RESERVED_DATA_CHANNEL_COUNT: u16 = 32;
const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(200);

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
}

#[derive(Debug)]
pub enum CoreEvent {
    SpawnTrack(TrackReceiver),
}

pub struct ParticipantCore {
    // Hot: touched on every packet
    pub rtc: Rtc,
    pub udp_batcher: Batcher,
    pub tcp_batcher: Batcher,
    pub downstream: DownstreamAllocator,

    // Warm: touched per poll cycle
    pub upstream: UpstreamAllocator,
    pub participant_id: entity::ParticipantId,

    // Cold: touched rarely
    pub events: Vec<CoreEvent>,
    disconnect_reason: Option<DisconnectReason>,
    signaling: Signaling,
    last_slow_poll: Instant,
}

impl ParticipantCore {
    pub fn new(
        manual_sub: bool,
        participant_id: entity::ParticipantId,
        mut rtc: Rtc,
        udp_batcher: Batcher,
        tcp_batcher: Batcher,
    ) -> Self {
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

        Self {
            participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
            upstream: UpstreamAllocator::new(),
            downstream: DownstreamAllocator::new(manual_sub),
            disconnect_reason: None,
            events: Vec::with_capacity(32),
            signaling: Signaling::new(cid),
            last_slow_poll: Instant::now(),
        }
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

    pub fn handle_udp_packet_batch(
        &mut self,
        batch: net::RecvPacketBatch,
        now: Instant,
    ) -> Option<Instant> {
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
                let _ = self.rtc.handle_input(Input::Receive(now.into(), recv));
            } else {
                tracing::warn!(src = %batch.src, "Dropping malformed UDP packet");
            }
        }

        self.poll()
    }

    pub fn handle_tick(&mut self) -> Option<Instant> {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now().into()));
        self.poll()
    }

    pub fn handle_available_tracks(&mut self, tracks: &HashMap<entity::TrackId, TrackReceiver>) {
        for track_handle in tracks.values() {
            if track_handle.meta.origin_participant != self.participant_id {
                self.downstream.add_track(track_handle.clone());
            }
        }
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
        // This is used to self-healing state drifting especially when a participant has just
        // reconnected. The viewer won't get a notification, so we have to be proactive.
        self.signaling.reconcile(&mut self.downstream);
        self.update_desired_bitrate();
    }

    pub fn remove_available_tracks(&mut self, tracks: &HashMap<entity::TrackId, TrackReceiver>) {
        for track in tracks.values() {
            self.downstream.remove_track(track);
        }
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
        self.update_desired_bitrate();
    }

    pub fn poll_slow(&mut self, now: Instant) {
        self.update_desired_bitrate();
        self.downstream.poll_slow(now);
        self.upstream.poll_slow(now);
    }

    /// The Main Orchestrator.
    /// Drives the feedback loop between the RTC Engine and the Signaling Logic.
    pub fn poll(&mut self) -> Option<Instant> {
        let now = Instant::now();

        if now >= self.last_slow_poll + SLOW_POLL_INTERVAL {
            self.poll_slow(now);
            self.last_slow_poll = now;
        }

        loop {
            let rtc_deadline = self.poll_rtc()?;
            let did_work = self.signaling.poll(&mut self.rtc, &self.downstream);
            if did_work {
                // Signaling wrote data. The RTC engine is now "dirty" (has output to send).
                // We loop back to `poll_rtc` immediately to flush `Output::Transmit`.
                continue;
            }

            let next_slow_poll = self.last_slow_poll + SLOW_POLL_INTERVAL;

            // No new work generated. We are synced. Return the RTC deadline.
            return Some(rtc_deadline.min(next_slow_poll));
        }
    }

    /// Internal helper: Drains the RTC engine until it yields a Timeout.
    /// Handles Transmits (UDP/TCP) and Events (Logic).
    fn poll_rtc(&mut self) -> Option<Instant> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => return Some(deadline.into()),
                Ok(Output::Transmit(tx)) => match tx.proto {
                    Protocol::Udp => self.udp_batcher.push_back(tx.destination, &tx.contents),
                    Protocol::Tcp => self.tcp_batcher.push_back(tx.destination, &tx.contents),
                    _ => {}
                },
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
        let pt = match self
            .rtc
            .media(mid)
            .and_then(|m| m.remote_pts().first().copied())
        {
            Some(pt) => pt,
            None => return,
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
                if let Some((_current, desired)) = self.downstream.update_bitrate(available) {
                    self.rtc.bwe().set_desired_bitrate(desired);
                }
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

    fn update_desired_bitrate(&mut self) {
        if let Some((_current, desired)) = self.downstream.update_allocations() {
            self.rtc.bwe().set_desired_bitrate(desired);
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = self.participant_id.derive_track_id(media.kind, &media.mid);
                let track_meta = Arc::new(track::TrackMeta {
                    id: track_id,
                    origin_participant: self.participant_id,
                    kind: media.kind,
                    simulcast_rids: media
                        .simulcast
                        .map(|s| s.recv.iter().map(|l| l.rid).collect()),
                });
                let (tx, rx) = track::new(media.mid, track_meta);
                let accepted = self.upstream.add_published_track(media.mid, tx);
                if !accepted {
                    self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                }
                self.events.push(CoreEvent::SpawnTrack(rx));
            }
            Direction::SendOnly => {
                self.downstream.add_slot(media.mid, media.kind);
                self.signaling
                    .set_slot_count(self.downstream.video.slot_count());
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
