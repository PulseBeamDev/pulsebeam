use std::{collections::HashMap, sync::Arc, task::Poll};

use pulsebeam_runtime::{prelude::*, rt};
use str0m::{Rtc, RtcError, error::SdpError, media::KeyframeRequest};
use tokio_stream::StreamExt;

use crate::{
    entity, gateway, message, node,
    participant::{
        batcher::{Batcher, BatcherState},
        core::ParticipantCore,
        effect::Effect,
    },
    room, track,
};
use pulsebeam_runtime::{actor, mailbox, net};

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
pub enum ParticipantDataMessage {}

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<entity::ParticipantId>;
    type ObservableState = ();
}

/// The async driver for a `ParticipantCore`.
pub struct ParticipantActor {
    core: ParticipantCore,
    effects_buffer: Vec<Effect>,
    // Async-specific handles
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    egress: Arc<net::UnifiedSocket>,
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
        let ufrag = self.core.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = mailbox::new(256);
        let _ = self
            .node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;

        loop {
            // --- DRAIN & PROCESS (Synchronous) ---
            self.drain_inputs(ctx, &mut gateway_rx);
            self.drain_keyframe_requests();
            let Some(delay) = self.core.tick() else {
                break; // Core requested shutdown.
            };

            // Flush all pending packets. This is the only place we should await egress I/O.
            // If the network is congested, we will block here, but that's okay because
            // we have already processed all other pending inputs for this tick.
            self.core.batcher.flush(&self.egress);

            // --- APPLY EFFECTS (Asynchronous) ---
            self.apply_core_effects().await;

            tokio::select! {
                biased;
                Some(msg) = ctx.hi_rx.recv() => self.handle_control_message(msg),
                Some(msg) = ctx.lo_rx.recv() => {},
                Some((meta, rtp)) = self.core.downstream_manager.next() => {
                    self.core.handle_forward_rtp(meta, &rtp.value);
                }
                Some(pkt) = gateway_rx.recv() => self.core.handle_udp_packet(pkt),
                _ = rt::sleep(delay) => { /* Timeout expired. */ },
            }
        }

        tracing::info!(participant_id = %self.meta(), "Shutting down actor.");
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
        let batcher = Batcher::with_capacity(gso_segments * MAX_MTU);
        let core = ParticipantCore::new(participant_id, rtc, batcher);

        Self {
            core,
            effects_buffer: Vec::with_capacity(32),
            node_ctx,
            room_handle,
            egress,
        }
    }

    /// Greedily drains all input channels and forwards them to the synchronous core.
    fn drain_inputs(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
        gateway_rx: &mut mailbox::Receiver<net::RecvPacket>,
    ) {
        let start = tokio::time::Instant::now();

        while let Ok(msg) = ctx.hi_rx.try_recv() {
            self.handle_control_message(msg);
        }
        // while let Ok(msg) = ctx.lo_rx.try_recv() {
        //     self.handle_data_message(msg);
        // }
        while let Ok(pkt) = gateway_rx.try_recv() {
            self.core.handle_udp_packet(pkt);
        }
        while let Poll::Ready(Some((meta, slot))) = self.core.downstream_manager.poll_next_packet()
        {
            self.core.handle_forward_rtp(meta, &slot.value);
        }

        let elapsed = start.elapsed().as_micros();
        let labels = [("type", "drain_inputs")];
        metrics::histogram!("participant_poll_delay_us", &labels).record(elapsed as f64);
    }

    fn drain_keyframe_requests(&mut self) {
        // TODO: make this reactive and encapsulate in core
        for track in &mut self.core.published_tracks {
            for sender in &mut track.1.simulcast {
                let Some(key) = sender.get_keyframe_request() else {
                    continue;
                };

                let mut api = self.core.rtc.direct_api();
                let Some(stream) = api.stream_rx_by_mid(key.request.mid, key.request.rid) else {
                    tracing::warn!("stream_rx not found, keyframe request failed");
                    return;
                };

                stream.request_keyframe(key.request.kind);
                tracing::debug!(?key, "requested keyframe");
            }
        }
    }

    /// Asynchronously executes all side effects produced by the core.
    async fn apply_core_effects(&mut self) {
        let start = tokio::time::Instant::now();
        self.effects_buffer.extend(self.core.effects.drain(..));

        for effect in self.effects_buffer.drain(..) {
            match effect {
                Effect::SpawnTrack(track_meta) => {
                    let (tx, rx) = track::new(track_meta, 64);
                    self.core.add_published_track(tx);
                    let _ = self
                        .room_handle
                        .send_high(room::RoomMessage::PublishTrack(rx))
                        .await;
                }
                Effect::Disconnect => {
                    self.core.rtc.disconnect();
                }
                Effect::Subscribe(track) => {
                    self.core.subscribe_track(track);
                }
            }
        }

        let elapsed = start.elapsed().as_micros();
        let labels = [("type", "effects")];
        metrics::histogram!("participant_poll_delay_us", &labels).record(elapsed as f64);
    }

    fn handle_control_message(&mut self, msg: ParticipantControlMessage) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.core.handle_published_tracks(&tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.core.handle_published_tracks(&tracks);
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core.remove_available_tracks(&tracks);
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {}
        }
    }
}

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
