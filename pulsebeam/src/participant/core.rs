use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling;
use pulsebeam_runtime::net::{self, Transport};
use std::collections::HashMap;
use std::sync::Arc;
use str0m::bwe::BweKind;
use str0m::channel::ChannelId;
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use str0m::net::Protocol;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    media::{Direction, MediaAdded},
};
use tokio::time::Instant;

use crate::entity;
use crate::participant::{
    batcher::Batcher, downstream::DownstreamAllocator, upstream::UpstreamAllocator,
};
use crate::rtp::RtpPacket;
use crate::track::{self, TrackReceiver};

const SIGNALING_CHANNEL_LABEL: &str = "__internal/v1/signaling";

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
    pub udp_batcher: Batcher,
    pub tcp_batcher: Batcher,
    pub upstream: UpstreamAllocator,
    pub downstream: DownstreamAllocator,
    pub events: Vec<CoreEvent>,
    disconnect_reason: Option<DisconnectReason>,

    signaling_cid: Option<ChannelId>,
}

impl ParticipantCore {
    pub fn new(
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
        udp_batcher: Batcher,
        tcp_batcher: Batcher,
    ) -> Self {
        Self {
            participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
            upstream: UpstreamAllocator::new(),
            downstream: DownstreamAllocator::new(),
            disconnect_reason: None,
            events: Vec::with_capacity(32),
            signaling_cid: None,
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

    pub fn handle_udp_packet_batch(&mut self, batch: net::RecvPacketBatch) -> Option<Instant> {
        let mut last_deadline = None;
        let transport = match batch.transport {
            Transport::Udp(_) => str0m::net::Protocol::Udp,
            Transport::Tcp => str0m::net::Protocol::Tcp,
            _ => str0m::net::Protocol::Udp,
        };
        for pkt in batch.into_iter() {
            if let Ok(contents) = (*pkt).try_into() {
                let recv = str0m::net::Receive {
                    proto: transport,
                    source: batch.src,
                    destination: batch.dst,
                    contents,
                };
                let _ = self
                    .rtc
                    .handle_input(Input::Receive(Instant::now().into(), recv));
                last_deadline = self.poll_rtc();
            } else {
                tracing::warn!(src = %batch.src, "Dropping malformed UDP packet");
            }
        }

        last_deadline
    }

    pub fn handle_timeout(&mut self) -> Option<Instant> {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now().into()));
        self.poll_rtc()
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
        for track in tracks.values() {
            self.downstream.remove_track(track);
        }
        self.update_desired_bitrate();
    }

    pub fn poll_slow(&mut self, now: Instant) {
        self.update_desired_bitrate();
        self.downstream.poll_slow(now);
        self.upstream.poll_slow(now);
    }

    pub fn poll_rtc(&mut self) -> Option<Instant> {
        if self.disconnect_reason.is_some() {
            return None;
        }

        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.into());
                }
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
            pkt.payload.to_vec(),
        ) {
            tracing::warn!(%mid, ssrc = %pkt.raw_header.ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }

    fn reconcile_states(&mut self) {
        let Some(cid) = self.signaling_cid else {
            return;
        };

        let Some(mut ch) = self.rtc.channel(cid) else {
            return;
        };

        // TODO: handle audio tracks
        let available_tracks = self
            .downstream
            .video
            .tracks()
            .map(|t| signaling::TrackInfo {
                track_id: t.id.to_string(),
                kind: signaling::TrackKind::Video.into(),
                participant_id: t.origin_participant.to_string(),
            })
            .collect();
        let subscriptions = self
            .downstream
            .video
            .slots()
            .map(|s| signaling::ActiveSubscription {
                mid: s.mid.to_string(),
                track: Some(signaling::TrackInfo {
                    track_id: s.track.id.to_string(),
                    kind: signaling::TrackKind::Video.into(),
                    participant_id: s.track.origin_participant.to_string(),
                }),
            })
            .collect();

        let states = signaling::ServerMessage {
            available_tracks: Some(signaling::AvailableTracksSnapshot {
                tracks: available_tracks,
            }),
            active_subscriptions: Some(signaling::ActiveSubscriptionsSnapshot { subscriptions }),
            error: None,
        };
        let buf = states.encode_to_vec();

        // TODO: handle reconcilation error
        ch.write(true, &buf);
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
                if let Some((current, desired)) = self.downstream.update_bitrate(available) {
                    self.rtc.bwe().set_current_bitrate(current);
                    self.rtc.bwe().set_desired_bitrate(desired);
                }
            }
            Event::ChannelOpen(cid, label) => {
                if label == SIGNALING_CHANNEL_LABEL {
                    self.signaling_cid = Some(cid);
                    self.reconcile_states();
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
        if let Some((current, desired)) = self.downstream.update_allocations() {
            self.rtc.bwe().set_current_bitrate(current);
            self.rtc.bwe().set_desired_bitrate(desired);
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = Arc::new(entity::TrackId::new());
                let track_meta = Arc::new(track::TrackMeta {
                    id: track_id,
                    origin_participant: self.participant_id.clone(),
                    kind: media.kind,
                    simulcast_rids: media
                        .simulcast
                        .map(|s| s.recv.iter().map(|l| l.rid).collect()),
                });
                let (tx, rx) = track::new(media.mid, track_meta, 128);
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
