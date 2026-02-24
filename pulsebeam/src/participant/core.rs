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
pub(crate) const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(200);

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

// Align the entire struct to one cache line so the heap allocation from Box::new
// never straddles a boundary, and so that the first cache-line fetch always
// brings in the `rtc` header and both batcher structs together.  On x86-64 a
// cache line is 64 bytes; on Apple Silicon it is 128 bytes — 64-byte alignment
// is the safe portable choice and satisfies both.
#[repr(align(64))]
pub struct ParticipantCore {
    // ── HOT (touched on every packet / every poll_fast call) ────────────────
    // These fields are referenced in the tight inner loop:
    //   poll_rtc()            → rtc, udp_batcher, tcp_batcher
    //   handle_forward_rtp()  → rtc
    //   handle_udp_packet_batch() → rtc
    //   poll_fast()           → downstream.dirty_allocation, signaling
    //   needs_slow_poll()     → last_slow_poll  (every outer loop iteration)
    pub rtc: Rtc,
    pub udp_batcher: Batcher,
    pub tcp_batcher: Batcher,
    /// Timestamp of the last slow-poll run.  Sampled on every outer loop
    /// iteration via `needs_slow_poll()`, so it must live in the hot group.
    last_slow_poll: Instant,

    // ── WARM (touched per poll cycle, not per packet) ────────────────────────
    pub downstream: DownstreamAllocator,
    signaling: Signaling,
    pub upstream: UpstreamAllocator,

    // ── COLD (lifecycle; at most twice per participant) ──────────────────────
    pub participant_id: entity::ParticipantId,
    pub events: Vec<CoreEvent>,
    disconnect_reason: Option<DisconnectReason>,
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
            // hot group
            rtc,
            udp_batcher,
            tcp_batcher,
            last_slow_poll: Instant::now(),
            // warm group
            downstream: DownstreamAllocator::new(manual_sub),
            signaling: Signaling::new(cid),
            upstream: UpstreamAllocator::new(),
            // cold group
            participant_id,
            events: Vec::with_capacity(32),
            disconnect_reason: None,
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

        // Use poll_fast: slow poll is handled once per outer actor loop iteration,
        // not inside this hot-path arm.
        self.poll_fast()
    }

    pub fn handle_tick(&mut self) -> Option<Instant> {
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now().into()));
        // Use poll_fast: the timer arm fires frequently; slow poll runs separately.
        self.poll_fast()
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
    }

    pub fn remove_available_tracks(&mut self, tracks: &HashMap<entity::TrackId, TrackReceiver>) {
        for track in tracks.values() {
            self.downstream.remove_track(track);
        }
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
    }

    fn poll_slow(&mut self, now: Instant) {
        self.downstream.poll_slow(now, &mut self.rtc.bwe());
        self.upstream.poll_slow(now);
    }

    /// The Main Orchestrator.
    /// Drives the feedback loop between the RTC Engine and the Signaling Logic.
    ///
    /// Runs `poll_slow` when the interval has elapsed. Use `poll_fast` in
    /// hot-path arms where predictable latency is required.
    pub fn poll(&mut self) -> Option<Instant> {
        let now = Instant::now();

        if now >= self.last_slow_poll + SLOW_POLL_INTERVAL {
            self.poll_slow(now);
            self.last_slow_poll = now;
        }

        self.poll_fast()
    }

    /// Fast variant of `poll`: drives the RTC engine and signaling loop but
    /// **never** runs `poll_slow`. Call this inside hot drain arms (downstream
    /// RTP forwarding, UDP ingress) where bounded latency is required.
    /// `poll` (which includes `poll_slow`) is called once per outer loop
    /// iteration from the actor, ensuring slow work is never deferred indefinitely.
    pub fn poll_fast(&mut self) -> Option<Instant> {
        loop {
            let rtc_deadline = self.poll_rtc()?;
            let did_work = self.signaling.poll(&mut self.rtc, &self.downstream);
            if did_work {
                continue;
            }

            if self.downstream.dirty_allocation {
                self.downstream.update_allocations(&mut self.rtc.bwe());
                continue;
            }

            let next_slow_poll = self.last_slow_poll + SLOW_POLL_INTERVAL;
            return Some(rtc_deadline.min(next_slow_poll));
        }
    }

    /// Runs the slow poll (BWE, allocation, upstream health) and updates
    /// `last_slow_poll`. Call this at the top of the outer actor loop, not
    /// inside a select! drain arm.
    pub fn run_slow_poll(&mut self) -> Option<Instant> {
        let now = Instant::now();
        self.poll_slow(now);
        self.last_slow_poll = now;
        self.poll_fast()
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

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            Event::KeyframeRequest(req) => self.downstream.handle_keyframe_request(req),
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
