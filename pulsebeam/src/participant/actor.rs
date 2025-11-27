use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::{actor, mailbox, net};
use pulsebeam_runtime::{prelude::*, rt};
use str0m::{Rtc, RtcError, error::SdpError};
use tokio::time::Instant;
use tokio_metrics::TaskMonitor;

use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::{entity, gateway, node, room, track};

// The hard limit of IPv4/IPv6 total packet size is 65535
// a bit lower to be less bursty and safer from possible off-by-one errors
const MAX_GSO_SIZE: usize = 65507;
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

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type Msg = ParticipantControlMessage;
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

    fn kind() -> ActorKind {
        "participant"
    }

    fn meta(&self) -> Arc<entity::ParticipantId> {
        self.core.participant_id.clone()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
    ) -> Result<(), actor::ActorError> {
        const BATCH_BUDGET: u8 = 64;
        let ufrag = self.core.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = mailbox::new(32);

        let _ = self
            .node_ctx
            .gateway
            .send(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;
        let mut stats_interval = tokio::time::interval(Duration::from_millis(200));
        let mut current_deadline = Instant::now() + Duration::from_secs(1);
        let rtc_timer = tokio::time::sleep_until(current_deadline);
        tokio::pin!(rtc_timer);
        let mut budget = 0;

        loop {
            let Some(new_deadline) = self.core.poll_rtc() else {
                break;
            };
            if new_deadline != current_deadline {
                rtc_timer.as_mut().reset(new_deadline);
                current_deadline = new_deadline;
            }

            let events: Vec<_> = self.core.drain_events().collect();
            for event in events {
                self.handle_core_event(event).await;
            }

            tokio::select! {
                biased;
                res = ctx.sys_rx.recv() => {
                    match res {
                        Some(msg) => match msg {
                            actor::SystemMsg::GetState(responder) => {
                                let _ = responder.send(self.get_observable_state());
                            }
                            actor::SystemMsg::Terminate => break,
                        },
                        None => break,
                    }
                }
                Some(msg) = ctx.rx.recv() => self.handle_control_message(msg).await,
                Ok(_) = self.egress.writable(), if !self.core.batcher.is_empty() => {
                    self.core.batcher.flush(&self.egress);
                },
                Some(pkt) = gateway_rx.recv() => self.core.handle_udp_packet(pkt),
                Some((meta, pkt)) = self.core.downstream.next() => {
                    self.core.handle_forward_rtp(meta, pkt);
                },
                now = stats_interval.tick() => {
                    self.core.poll_stats(now);
                }
                _ = &mut rtc_timer => {
                    self.core.handle_timeout();
                },
            }
            budget += 1;
            if budget >= BATCH_BUDGET {
                rt::yield_now().await;
                budget = 0;
            }
        }

        if let Some(reason) = self.core.disconnect_reason() {
            tracing::info!(participant_id = %self.meta(), %reason, "Shutting down actor due to disconnect.");
        } else {
            tracing::info!(participant_id = %self.meta(), "Shutting down actor.");
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
        let batch_size_limit = std::cmp::min(gso_segments * MAX_MTU, MAX_GSO_SIZE);
        let core = ParticipantCore::new(participant_id, rtc, batch_size_limit);
        Self {
            core,
            node_ctx,
            room_handle,
            egress,
        }
    }

    async fn handle_core_event(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::SpawnTrack(rx) => {
                let _ = self
                    .room_handle
                    .send(room::RoomMessage::PublishTrack(rx))
                    .await;
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
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
