use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use crate::gateway::GatewayWorkerHandle;
use crate::participant::batcher::Batcher;
use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::{entity, gateway, room, track};
use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::actor::{self, SystemMsg};
use pulsebeam_runtime::net::UnifiedSocketWriter;
use pulsebeam_runtime::prelude::*;
use str0m::{Rtc, RtcError, error::SdpError};
use tokio::time::Instant;
use tokio_metrics::TaskMonitor;

pub use crate::participant::core::TrackMapping;

const MIN_QUANTA: Duration = Duration::from_millis(1);

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
}

#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<entity::TrackId, track::TrackReceiver>),
    TracksPublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TracksUnpublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TrackPublishRejected(track::TrackReceiver),
}

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type Msg = ParticipantControlMessage;
    type Meta = entity::ParticipantId;
    type ObservableState = ();
}

pub struct ParticipantActor {
    // Boxed to keep ParticipantCore off the async state machine stack. The actor::run()
    // future stores `a: ParticipantActor` inline while also holding `a.run()` as __awaitee,
    // so every unboxed field adds directly to the task's memory footprint.
    core: Box<ParticipantCore>,
    gateway: GatewayWorkerHandle,
    udp_egress: UnifiedSocketWriter,
    tcp_egress: UnifiedSocketWriter,
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

    fn meta(&self) -> entity::ParticipantId {
        self.core.participant_id
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
    ) -> Result<(), actor::ActorError> {
        let ufrag = self.core.rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, mut gateway_rx) = pulsebeam_runtime::sync::mpsc::channel(256);

        let _ = self
            .gateway
            .send(gateway::GatewayControlMessage::AddParticipant(
                ufrag.clone(),
                gateway_tx,
            ))
            .await;
        let sleep = tokio::time::sleep(MIN_QUANTA);
        tokio::pin!(sleep);

        let mut needs_poll = true;
        let mut maybe_deadline = None;
        let mut budget = 32;

        loop {
            if budget <= 0 {
                tokio::task::yield_now().await;
                budget = 32;
            }

            if !self.core.events.is_empty() {
                self.handle_control_message_tx().await;
            }

            if needs_poll {
                maybe_deadline = self.core.poll();
                self.core.udp_batcher.flush(&self.udp_egress);
                self.core.tcp_batcher.flush(&self.tcp_egress);
            }
            let now = Instant::now();
            if let Some(deadline) = maybe_deadline {
                // If the deadline is 'now' or in the past, we must not busy-wait.
                // We enforce a minimum 1ms "quanta" to prevent CPU starvation.
                let adjusted_deadline = if deadline <= now {
                    now + MIN_QUANTA
                } else {
                    deadline
                };
                if sleep.deadline() != adjusted_deadline {
                    sleep.as_mut().reset(adjusted_deadline);
                }
            } else {
                break;
            }

            needs_poll = true;
            tokio::select! {
                // Priority 1: Keyframe requests from upstream
                _ = self.core.upstream.notified() => {
                    let now = Instant::now();
                    let reqs: Vec<_> = self.core.upstream.drain_keyframe_requests(now).collect();
                    for req in reqs {
                        self.core.handle_keyframe_request(req);
                    }
                }

                // Priority 2: Ingress — incoming network packets drive the RTC state machine.
                // Must be above egress; otherwise a fully-loaded downstream starves the
                // ingress path and the RTC engine never processes NACKs / ICE / DTLS.
                Ok(batch) = gateway_rx.recv() => {
                    needs_poll = false;
                    maybe_deadline = self.core.handle_udp_packet_batch(batch, now);
                },

                // Priority 3: Egress — only forward downstream RTP when ingress is empty.
                (meta, pkt) = self.core.downstream.next() => {
                    self.core.handle_forward_rtp(meta, pkt);
                },

                // Priority 4: Control Messages
                Some(msg) = ctx.sys_rx.recv() => {
                    self.handle_system_message_rx(msg);
                }
                Some(msg) = ctx.rx.recv() => {
                    self.handle_control_message_rx(msg);
                }

                // Priority 5: Background tasks
                _ = &mut sleep => {
                    self.core.handle_tick();
                },
            }
        }

        if let Some(reason) = self.core.disconnect_reason() {
            tracing::info!(participant_id = %self.meta(), %reason, "Shutting down actor due to disconnect.");
        } else {
            tracing::info!(participant_id = %self.meta(), "Shutting down actor.");
        }
        let _ = self
            .gateway
            .send(gateway::GatewayControlMessage::RemoveParticipant(ufrag))
            .await;
        Ok(())
    }
}

impl ParticipantActor {
    pub fn new(
        gateway_handle: GatewayWorkerHandle,
        room_handle: room::RoomHandle,
        udp_egress: UnifiedSocketWriter,
        tcp_egress: UnifiedSocketWriter,
        participant_id: entity::ParticipantId,
        rtc: Rtc,
        manual_sub: bool,
    ) -> Self {
        let udp_batcher = Batcher::with_capacity(udp_egress.max_gso_segments());
        let tcp_batcher = Batcher::with_capacity(tcp_egress.max_gso_segments());
        let core = Box::new(ParticipantCore::new(
            manual_sub,
            participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
        ));
        Self {
            gateway: gateway_handle,
            core,
            room_handle,
            udp_egress,
            tcp_egress,
        }
    }

    fn handle_system_message_rx(&mut self, msg: SystemMsg<()>) {
        match msg {
            actor::SystemMsg::GetState(responder) => {
                let _: () = self.get_observable_state();
                let _ = responder.send(());
            }
            actor::SystemMsg::Terminate => {
                self.core
                    .disconnect(super::core::DisconnectReason::SystemTerminated);
            }
        }
    }

    fn handle_control_message_rx(&mut self, msg: ParticipantControlMessage) {
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

    async fn handle_control_message_tx(&mut self) {
        let batch_size = self.core.events.len().min(16);
        let mut room_closed = false;

        for event in self.core.events.drain(..batch_size) {
            match event {
                CoreEvent::SpawnTrack(rx) => {
                    let res = self
                        .room_handle
                        .send(room::RoomMessage::PublishTrack(rx))
                        .await;

                    if res.is_err() {
                        room_closed = true;
                        break;
                    }
                }
            }
        }

        if room_closed {
            tracing::warn!("room is closed, exiting");
            self.core
                .disconnect(crate::participant::core::DisconnectReason::RoomClosed);
        }
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
