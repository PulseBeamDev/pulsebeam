use std::{collections::HashMap, sync::Arc};

use once_cell::sync::Lazy;
use pulsebeam_runtime::{actor, mailbox, net, rt};
use str0m::{Rtc, RtcError, error::SdpError};
use tokio_metrics::TaskMonitor;
use tokio_stream::StreamExt;

use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::{entity, gateway, node, room, track};

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

pub struct ParticipantActor {
    core: ParticipantCore,
    node_ctx: node::NodeContext,
    room_handle: room::RoomHandle,
    egress: Arc<net::UnifiedSocket>,
}

impl actor::Actor<ParticipantMessageSet> for ParticipantActor {
    fn monitor() -> Arc<TaskMonitor> {
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
        let (gateway_tx, mut gateway_rx) = mailbox::new(64);

        let _ = self
            .node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;

        'outer: loop {
            // 1. Advance the core's synchronous state machine.
            let Some(delay) = self.core.poll_rtc() else {
                break 'outer;
            };

            self.drain_keyframe_requests();

            // 2. Execute any asynchronous side effects requested by the core.
            let events: Vec<_> = self.core.drain_events().collect();
            for event in events {
                self.handle_core_event(event).await;
            }

            // 3. Wait for the next I/O event or timeout.
            tokio::select! {
                biased;
                Some(msg) = ctx.hi_rx.recv() => self.handle_control_message(msg).await,
                Ok(_) = self.egress.writable(), if !self.core.batcher.is_empty() => {
                    self.core.batcher.flush(&self.egress);
                },
                Some(pkt) = gateway_rx.recv() => self.core.handle_udp_packet(pkt),
                Some((meta, rtp)) = self.core.downstream_allocator.next() => {
                    self.core.handle_forward_rtp(meta, &rtp.value);
                },
                _ = rt::sleep(delay) => {
                    self.core.handle_timeout();
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
        let core = ParticipantCore::new(participant_id, rtc, gso_segments * MAX_MTU);
        Self {
            core,
            node_ctx,
            room_handle,
            egress,
        }
    }

    /// Executes asynchronous actions requested by the core.
    async fn handle_core_event(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::SpawnTrack(track_meta) => {
                let (tx, rx) = track::new(track_meta, 64);
                self.core.add_published_track(tx);
                let _ = self
                    .room_handle
                    .send_high(room::RoomMessage::PublishTrack(rx))
                    .await;
            }
            CoreEvent::Disconnect => {
                self.core.rtc.disconnect();
            }
        }
    }

    async fn handle_control_message(&mut self, msg: ParticipantControlMessage) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.core.handle_available_tracks(&tracks)
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.core.handle_available_tracks(&tracks)
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core.remove_available_tracks(&tracks)
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {}
        };
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
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
