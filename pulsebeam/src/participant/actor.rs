use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::net::UnifiedSocket;
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, net};
use str0m::{Rtc, RtcError, error::SdpError};
use tokio::time::Instant;
use tokio_metrics::TaskMonitor;

use crate::gateway::GatewayWorkerHandle;
use crate::participant::batcher::Batcher;
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
    gateway: GatewayWorkerHandle,
    udp_egress: Arc<UnifiedSocket>,
    tcp_egress: Arc<UnifiedSocket>,
    room_handle: room::RoomHandle,
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
        let ufrag = self.core.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = mailbox::new(64);

        let _ = self
            .gateway
            .send(gateway::GatewayControlMessage::AddParticipant(
                self.meta(),
                ufrag.clone(),
                gateway_tx,
            ))
            .await;
        let mut stats_interval = tokio::time::interval(Duration::from_millis(200));
        let mut current_deadline = Instant::now() + Duration::from_secs(1);
        let mut new_deadline = Some(current_deadline);

        let rtc_timer = tokio::time::sleep_until(current_deadline);
        tokio::pin!(rtc_timer);

        loop {
            match new_deadline {
                Some(new_deadline) if new_deadline != current_deadline => {
                    rtc_timer.as_mut().reset(new_deadline);
                    current_deadline = new_deadline;
                }
                None => break,
                _ => {}
            }

            let events: Vec<_> = self.core.drain_events().collect();
            for event in events {
                self.handle_core_event(event).await;
            }

            tokio::select! {
                biased;
                // Priority 1: Control Messages
                res = ctx.sys_rx.recv() => {
                    match res {
                        Some(msg) => match msg {
                            actor::SystemMsg::GetState(responder) => {
                                responder.send(self.get_observable_state());
                            }
                            actor::SystemMsg::Terminate => break,
                        },
                        None => break,
                    }
                }
                Some(msg) = ctx.rx.recv() => self.handle_control_message(msg).await,

                // Priority 2: CPU Work
                // TODO: consolidate pollings in core
                Some((_, req)) = self.core.upstream.keyframe_request_streams.next() => {
                    self.core.handle_keyframe_request(req);
                }
                Some((meta, pkt)) = self.core.downstream.next() => {
                    self.core.handle_forward_rtp(meta, pkt);
                    // this indicates the first batch is filled.
                    if self.core.udp_batcher.len() >= 2 {
                        self.core.udp_batcher.flush(&self.udp_egress);
                    }
                    if self.core.tcp_batcher.len() >= 2 {
                        self.core.tcp_batcher.flush(&self.tcp_egress);
                    }

                    new_deadline = self.core.poll_rtc();
                },
                Some(batch) = gateway_rx.recv() => {
                    new_deadline = self.core.handle_udp_packet_batch(batch);
                },

                // Priority 3: Flush to network
                Ok(_) = self.udp_egress.writable(), if !self.core.udp_batcher.is_empty() => {
                    self.core.udp_batcher.flush(&self.udp_egress);
                },
                Ok(_) = self.tcp_egress.writable(), if !self.core.tcp_batcher.is_empty() => {
                    self.core.tcp_batcher.flush(&self.tcp_egress);
                },

                // Priority 4: Background tasks
                now = stats_interval.tick() => {
                    self.core.poll_slow(now);
                }
                _ = &mut rtc_timer => {
                    new_deadline = self.core.handle_timeout();
                },
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
        gateway_handle: GatewayWorkerHandle,
        room_handle: room::RoomHandle,
        udp_egress: Arc<UnifiedSocket>,
        tcp_egress: Arc<UnifiedSocket>,
        participant_id: Arc<entity::ParticipantId>,
        rtc: Rtc,
    ) -> Self {
        let udp_batcher = {
            let gso_segments = udp_egress.max_gso_segments();
            let batch_size_limit = std::cmp::min(gso_segments * MAX_MTU, MAX_GSO_SIZE);
            Batcher::with_capacity(batch_size_limit)
        };
        let tcp_batcher = {
            let gso_segments = tcp_egress.max_gso_segments();
            let batch_size_limit = std::cmp::min(gso_segments * MAX_MTU, MAX_GSO_SIZE);
            Batcher::with_capacity(batch_size_limit)
        };
        let core = ParticipantCore::new(participant_id, rtc, udp_batcher, tcp_batcher);
        Self {
            gateway: gateway_handle,
            core,
            room_handle,
            udp_egress,
            tcp_egress,
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
