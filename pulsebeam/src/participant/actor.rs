use std::{collections::HashMap, sync::Arc, time::Duration};

use pulsebeam_runtime::{prelude::*, rt};
use str0m::{
    Rtc, RtcError,
    error::SdpError,
    media::{KeyframeRequest, MediaKind},
};
use tokio::time::Instant;

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

// --- Tuning Constants ---
const RTC_POLL_BUDGET: usize = 64;
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

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type HighPriorityMsg = ParticipantControlMessage;
    type LowPriorityMsg = ParticipantDataMessage;
    type Meta = Arc<entity::ParticipantId>;
    type ObservableState = ();
}

/// The async driver for a `ParticipantCore`.
///
/// This actor's responsibility is to bridge the async world (networking, channels)
/// with the synchronous `ParticipantCore` state machine. It follows a strict
/// "drain-process-apply-await" loop to ensure fairness and performance.
pub struct ParticipantActor {
    core: ParticipantCore,
    // Async-specific state and handles
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    egress: Arc<net::UnifiedSocket>,
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
            let Some(delay) = self.process_engine() else {
                break;
            };

            // --- APPLY EFFECTS (Asynchronous) ---
            self.apply_core_effects().await;

            // --- FLUSH NETWORK & AWAIT NEXT EVENT ---
            self.core.batcher.flush(&self.egress);

            tokio::select! {
                biased;
                Some(msg) = ctx.hi_rx.recv() => self.handle_control_message(msg),
                Some(msg) = ctx.lo_rx.recv() => self.handle_data_message(msg),
                Some(pkt) = gateway_rx.recv() => self.handle_udp_packet(pkt),
                _ = rt::sleep(delay) => {
                    // Timeout expired. The next loop will handle it.
                },
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
            node_ctx,
            room_handle,
            egress,
            subscribed_tracks: HashMap::new(),
        }
    }

    /// Greedily drains all input channels and forwards them to the synchronous core.
    fn drain_inputs(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
        gateway_rx: &mut mailbox::Receiver<net::RecvPacket>,
    ) {
        while let Ok(msg) = ctx.hi_rx.try_recv() {
            self.handle_control_message(msg);
        }
        while let Ok(msg) = ctx.lo_rx.try_recv() {
            self.handle_data_message(msg);
        }
        while let Ok(pkt) = gateway_rx.try_recv() {
            self.handle_udp_packet(pkt);
        }
        self.process_media_ingress();
    }

    /// Drives the synchronous `ParticipantCore` engine with a budget.
    fn process_engine(&mut self) -> Option<Duration> {
        for _ in 0..RTC_POLL_BUDGET {
            match self.core.poll_engine() {
                Ok(Some(delay)) => {
                    return Some(delay);
                }
                Ok(_) => continue, // Transmit and Event effects are queued in the core.
                Err(_) => {
                    self.core.rtc.disconnect();
                    break;
                }
            }
        }
        None
    }

    /// Asynchronously executes all side effects produced by the core.
    async fn apply_core_effects(&mut self) {
        for effect in self.core.effects.drain(..) {
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
                // Other effects like requesting keyframes would be handled here.
                _ => {}
            }
        }
    }

    fn handle_udp_packet(&mut self, packet: net::RecvPacket) {
        let contents = match (&*packet.buf).try_into() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Invalid UDP packet size, dropping: {}", e);
                return;
            }
        };
        let recv = str0m::net::Receive {
            proto: str0m::net::Protocol::Udp,
            source: packet.src,
            destination: packet.dst,
            contents,
        };
        self.core.handle_udp_packet(Instant::now(), recv);
    }

    fn handle_control_message(&mut self, msg: ParticipantControlMessage) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.core.handle_published_tracks(&tracks);
                self.subscribe_to_tracks(tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                let tracks = Arc::try_unwrap(tracks).unwrap_or_else(|arc| (*arc).clone());
                self.core.handle_published_tracks(&tracks);
                self.subscribe_to_tracks(tracks);
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core.remove_available_tracks(&tracks);
                for track_id in tracks.keys() {
                    self.subscribed_tracks.remove(track_id);
                }
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {}
        }
    }

    fn handle_data_message(&mut self, msg: ParticipantDataMessage) {
        match msg {
            ParticipantDataMessage::KeyframeRequest(track_id, req) => {
                // In a future step, this could become an Effect produced by the core.
                // For now, the actor can still manage this.
                // let _ = self
                //     .room_handle
                //     .send_low(room::RoomMessage::KeyframeRequest { track_id, req });
            }
        }
    }

    fn subscribe_to_tracks(&mut self, tracks: HashMap<Arc<entity::TrackId>, track::TrackReceiver>) {
        for (track_id, track) in tracks {
            if track.meta.id.origin_participant != self.core.participant_id
                && track.meta.kind == MediaKind::Video
            {
                if !self.subscribed_tracks.contains_key(&track_id) {
                    tracing::info!(participant_id = %self.core.participant_id, track_id = %track_id, "Subscribing to new video track");
                    self.subscribed_tracks.insert(track_id, track);
                }
            }
        }
    }

    /// Drains media from subscribed tracks and forwards it into the core.
    fn process_media_ingress(&mut self) {
        for track in self.subscribed_tracks.values() {
            for receiver in track.simulcast.iter_mut() {
                while let Ok(Some(pkt)) = receiver.channel.try_recv() {
                    self.core.handle_forward_rtp(track.meta.clone(), pkt);
                }
            }
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
