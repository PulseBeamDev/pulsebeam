use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::prelude::*;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    error::SdpError,
    media::{KeyframeRequest, MediaKind},
    rtp::RtpPacket,
};
use tokio::time::Instant;

use crate::{
    entity, gateway, message, node,
    participant::{
        batcher::{Batcher, BatcherState},
        core::ParticipantCore,
        effect::{self, Effect},
    },
    room, track,
};
use pulsebeam_runtime::{actor, mailbox, net};

/// The interval at which the actor loop will run to flush media, even if no
/// other events have occurred. This is the primary mechanism for ensuring a
/// low p99 latency target for media forwarding.
const PACING_INTERVAL: Duration = Duration::from_millis(1);

/// The maximum transmission unit for a single packet. Used to calculate batcher capacity.
const MAX_MTU: usize = 1500;

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
}

#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<Arc<entity::TrackId>, track::TrackReceiver>),
    TracksPublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackReceiver>>),
    TracksUnpublished(Arc<HashMap<Arc<entity::TrackId>, track::TrackReceiver>>),
    TrackPublishRejected(track::TrackReceiver),
}

#[derive(Debug)]
pub enum ParticipantDataMessage {
    KeyframeRequest(Arc<entity::TrackId>, message::KeyframeRequest),
}

pub struct ParticipantContext {
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    rtc: Rtc,
    egress: Arc<net::UnifiedSocket>,
}

impl ParticipantContext {
    async fn apply_effects(&mut self, core: &mut ParticipantCore, effects: &mut effect::Queue) {
        if effects.is_empty() {
            return;
        }

        for e in effects.drain(..) {
            match e {
                Effect::SpawnTrack(track_meta) => {
                    let (tx, rx) = track::new(track_meta, 64);
                    core.add_published_track(tx);

                    if let Err(e) = self
                        .room_handle
                        .send_high(room::RoomMessage::PublishTrack(rx))
                        .await
                    {
                        tracing::error!("Failed to publish track: {}", e);
                    }
                }
                Effect::Disconnect => {
                    self.rtc.disconnect();
                }
                _ => {}
            }
        }
    }
}

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<entity::ParticipantId>;
    type ObservableState = ();
}

/// The SFU participant actor: manages the WebRTC connection for a single peer.
///
/// This actor is designed around a batch-oriented main loop to maximize
/// performance and ensure low-latency media forwarding. It operates in phases:
/// 1. Greedily poll all input sources (control messages, media packets).
/// 2. Process the collected batch of work, updating internal state.
/// 3. Flush all generated output packets in a single, efficient I/O operation.
/// 4. Await new events or a pacing timeout.
pub struct ParticipantActor {
    ctx: ParticipantContext,
    core: ParticipantCore,
    effects: VecDeque<effect::Effect>,
    batcher: Batcher,
    /// All tracks this participant is subscribed to and actively forwarding.
    subscribed_tracks: HashMap<Arc<entity::TrackId>, track::TrackReceiver>,
}

impl actor::Actor<ParticipantMessageSet> for ParticipantActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn meta(&self) -> Arc<entity::ParticipantId> {
        self.core.participant_id.clone()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
    ) -> Result<(), actor::ActorError> {
        let ufrag = self.ctx.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = mailbox::new(64);
        // It's acceptable to ignore the result here. If the gateway is unavailable,
        // the participant will simply fail to connect and eventually time out.
        let _ = self
            .ctx
            .node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;

        loop {
            // --- PHASE 1: Synchronous Work - Poll all inputs ---
            self.process_control_messages(ctx);
            self.process_media_ingress();
            self.process_network_input(&mut gateway_rx);
            let Some(rtc_deadline) = self.poll_rtc_engine() else {
                break;
            };

            if !self.ctx.rtc.is_alive() {
                tracing::info!(participant_id = %self.core.participant_id, "Participant disconnected, shutting down actor.");
                break;
            }

            // --- PHASE 2: Synchronous Work - Flush all outputs ---
            self.flush_egress();
            self.ctx
                .apply_effects(&mut self.core, &mut self.effects)
                .await;

            // --- PHASE 3: Asynchronous Wait ---
            let sleep_duration = rtc_deadline.min(PACING_INTERVAL);

            tokio::select! {
                biased;

                // Wakes when the network socket is ready to accept more data. This is
                // active whenever the batcher has items, as the head of the queue
                // might be blocked.
                Ok(_) = self.ctx.egress.writable(), if !self.batcher.is_empty() => {
                    // No action needed here; the next loop iteration will call flush_egress().
                }

                Some(msg) = ctx.hi_rx.recv() => {
                    self.on_high_priority_sync(ctx, msg);
                }
                _ = tokio::time::sleep(sleep_duration) => {}
            }
        }
        Ok(())
    }
}

impl ParticipantActor {
    pub fn new(
        node_ctx: node::NodeContext,
        room_handle: room::RoomHandle,
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        let egress = node_ctx.allocate_egress();
        let gso_segments = egress.max_gso_segments();
        let ctx = ParticipantContext {
            node_ctx,
            room_handle,
            rtc,
            egress,
        };
        let core = ParticipantCore::new(participant_id);

        Self {
            ctx,
            core,
            effects: VecDeque::with_capacity(16),
            batcher: Batcher::with_capacity(gso_segments * MAX_MTU),
            subscribed_tracks: HashMap::new(),
        }
    }

    /// Synchronously polls the `Rtc` engine, handling events and queueing
    /// outgoing packets in the batcher until it requests a timeout.
    fn poll_rtc_engine(&mut self) -> Option<Duration> {
        while self.ctx.rtc.is_alive() {
            match self.ctx.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = tokio::time::Instant::now().into_std();
                    let duration = deadline.saturating_duration_since(now);
                    if duration.is_zero() {
                        let _ = self.ctx.rtc.handle_input(Input::Timeout(now));
                        continue;
                    }
                    return Some(duration);
                }
                Ok(Output::Transmit(tx)) => self.batcher.push_back(tx.destination, &tx.contents),
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(e) => {
                    tracing::error!("RTC poll error: {}", e);
                    self.ctx.rtc.disconnect();
                }
            }
        }
        None
    }

    /// Greedily processes all pending high- and low-priority messages from the mailboxes.
    fn process_control_messages(&mut self, ctx: &mut actor::ActorContext<ParticipantMessageSet>) {
        while let Ok(msg) = ctx.hi_rx.try_recv() {
            self.on_high_priority_sync(ctx, msg);
        }
        while let Ok(msg) = ctx.lo_rx.try_recv() {
            match msg {
                ParticipantDataMessage::KeyframeRequest(id, req) => {
                    self.request_keyframe(KeyframeRequest {
                        mid: id.origin_mid,
                        kind: req.kind,
                        rid: req.rid,
                    });
                }
            }
        }
    }

    fn on_high_priority_sync(
        &mut self,
        _ctx: &mut actor::ActorContext<ParticipantMessageSet>,
        msg: ParticipantControlMessage,
    ) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.subscribe_to_tracks(tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.subscribe_to_tracks((*tracks).clone());
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core
                    .remove_available_tracks(&mut self.effects, tracks.as_ref());
                for track_id in tracks.keys() {
                    self.subscribed_tracks.remove(track_id);
                }
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {
                // Application-specific logic could be added here (e.g., notify client).
            }
        }
    }

    /// Greedily drains all subscribed media tracks using non-blocking receives
    /// and forwards the RTP packets into the RTC engine.
    fn process_media_ingress(&mut self) {
        let mut packets_to_forward = Vec::new();

        // Collect all available packets first to avoid borrow checker issues with `self`.
        // This requires `try_recv` on the spmc channel to take `&self`, not `&mut self`.
        // This is a necessary change in the spmc channel for this pattern to work.
        for track in self.subscribed_tracks.values_mut() {
            for simulcast_receiver in track.simulcast.iter_mut() {
                while let Ok(Some(pkt)) = simulcast_receiver.channel.try_recv() {
                    packets_to_forward.push((track.meta.clone(), pkt));
                }
            }
        }

        // Now, process the collected batch of packets.
        for (meta, pkt) in packets_to_forward {
            self.handle_forward_rtp(meta, pkt);
        }
    }

    fn process_network_input(&mut self, gateway_rx: &mut mailbox::Receiver<net::RecvPacket>) {
        while let Ok(pkt) = gateway_rx.try_recv() {
            self.handle_udp_packet(pkt);
        }
    }

    /// Flushes all pending egress packets in the batcher. If the socket is blocked,
    /// it caches the unsent batch to be retried on the next iteration.
    fn flush_egress(&mut self) {
        while let Some(state) = self.batcher.front() {
            if self.ctx.egress.try_send_batch(&net::SendPacketBatch {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            }) {
                let state = self.batcher.pop_front().unwrap();
                self.batcher.reclaim(state);
            } else {
                break;
            }
        }
    }

    /// Handles an incoming UDP packet from the gateway.
    fn handle_udp_packet(&mut self, packet: net::RecvPacket) {
        let contents = match (&*packet.buf).try_into() {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!("Invalid UDP packet size, dropping: {err}");
                return;
            }
        };

        let recv = str0m::net::Receive {
            proto: str0m::net::Protocol::Udp,
            source: packet.src,
            destination: packet.dst,
            contents,
        };
        let input = Input::Receive(Instant::now().into_std(), recv);
        if let Err(e) = self.ctx.rtc.handle_input(input) {
            tracing::error!("Rtc::handle_input error: {}", e);
        }
    }

    /// Handles events emitted by the `Rtc` engine.
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => {
                if state == str0m::IceConnectionState::Disconnected {
                    self.ctx.rtc.disconnect();
                }
            }
            Event::MediaAdded(media) => self.core.handle_media_added(&mut self.effects, media),
            Event::RtpPacket(rtp) => self.handle_rtp_packet(rtp),
            Event::KeyframeRequest(req) => self.handle_keyframe_request(req),
            Event::Connected => {
                tracing::info!(participant_id = %self.core.participant_id, "WebRTC connected.")
            }
            _ => {}
        }
    }

    /// Handles an RTP packet received from the peer, forwarding it to the correct local track.
    fn handle_rtp_packet(&mut self, mut rtp: RtpPacket) {
        let mut api = self.ctx.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            return;
        };
        let Some(track) = self.core.get_published_track_mut(&stream.mid()) else {
            return;
        };

        rtp.header.ext_vals.rid = stream.rid();
        track.send(stream.rid().as_ref(), rtp);
    }

    /// Forwards a request for a keyframe from a subscriber to the original publisher.
    fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        // TODO: rework keyframe requests
        if let Some(track_handle) = self.core.video_allocator.get_track_mut(&req.mid) {
            // let _ = self
            //     .ctx
            //     .room_handle
            //     .send_low(room::RoomMessage::KeyframeRequest {
            //         track_id: track_handle.meta.id.clone(),
            //         req: message::KeyframeRequest {
            //             rid: req.rid,
            //             kind: req.kind,
            //         },
            //     });
        }
    }

    /// Handles an RTP packet that has been forwarded from another participant's track.
    fn handle_forward_rtp(&mut self, track_meta: Arc<track::TrackMeta>, rtp: Arc<RtpPacket>) {
        let Some(mid) = self.core.get_slot(&track_meta, &rtp.header.ext_vals) else {
            tracing::debug!(
                track = ?track_meta,
                "no slot found for track; dropping forwarded RTP"
            );
            return;
        };
        let Some(media) = self.ctx.rtc.media(mid) else {
            tracing::warn!(%mid, "no media found for mid; dropping forwarded RTP");
            return;
        };
        let Some(&pt) = media.remote_pts().first() else {
            tracing::warn!(
                %mid,
                "no negotiated payload type for mid; dropping forwarded RTP"
            );
            return;
        };

        let mut api = self.ctx.rtc.direct_api();
        let Some(writer) = api.stream_tx_by_mid(mid, None) else {
            tracing::warn!(
                %mid,
                "no TxStream for mid; cannot forward RTP (maybe unnegotiated transceiver)"
            );
            return;
        };

        if let Err(err) = writer.write_rtp(
            pt,
            rtp.seq_no,
            rtp.header.timestamp,
            rtp.timestamp,
            rtp.header.marker,
            rtp.header.ext_vals.clone(),
            true,
            rtp.payload.clone(),
        ) {
            tracing::error!(%mid, %err, "failed to forward RTP packet");
        } else {
            tracing::trace!(%mid, "write_rtp succeeded");
        }
    }

    /// Sends a keyframe request to the remote peer for a specific media stream.
    fn request_keyframe(&mut self, req: KeyframeRequest) {
        let mut api = self.ctx.rtc.direct_api();
        if let Some(stream) = api.stream_rx_by_mid(req.mid, req.rid) {
            stream.request_keyframe(req.kind);
        }
    }

    /// Helper to process a new set of tracks to subscribe to.
    fn subscribe_to_tracks(&mut self, tracks: HashMap<Arc<entity::TrackId>, track::TrackReceiver>) {
        // TODO: refactor this to core
        tracing::debug!(?tracks, "subscribe to tracks");
        self.core
            .handle_published_tracks(&mut self.effects, &tracks);
        for (track_id, track) in tracks {
            if track.meta.kind == MediaKind::Video
                && track.meta.id.origin_participant != self.core.participant_id
            {
                self.subscribed_tracks.insert(track_id, track);
            }
        }
    }
}

/// A handle to the `ParticipantActor` for sending messages.
pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;

impl<'a> From<&'a BatcherState> for net::SendPacketBatch<'a> {
    fn from(state: &'a BatcherState) -> Self {
        Self {
            dst: state.dst,
            buf: &state.buf,
            segment_size: state.segment_size,
        }
    }
}
