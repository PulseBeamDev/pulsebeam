use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use crate::gateway::GatewayWorkerHandle;
use crate::participant::batcher::Batcher;
use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::{entity, gateway, room, track};
use futures_util::FutureExt;
use pulsebeam_runtime::actor;
use pulsebeam_runtime::actor::ActorKind;
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
        let room_handle = self.room_handle.clone();

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

        loop {
            if needs_poll {
                maybe_deadline = self.core.poll();

                // this indicates the first batch is filled.
                if self.core.udp_batcher.len() >= 2 {
                    self.core.udp_batcher.flush(&self.udp_egress);
                }
                if self.core.tcp_batcher.len() >= 2 {
                    self.core.tcp_batcher.flush(&self.tcp_egress);
                }
                needs_poll = false;
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
            }

            // Default: assume any wakeup needs an RTC poll.
            // Branches that only do egress I/O (no new RTC input) clear this flag themselves.
            needs_poll = true;
            let batch_size = self.core.events.len().min(16);

            tokio::select! {
                biased;
                // Priority 1: Egress Work
                // TODO: consolidate pollings in core
                _ = self.core.upstream.notified() => {
                    let now = Instant::now();
                    // Collect first to release the borrow on upstream before handle_keyframe_request
                    // borrows all of core.  Allocations here are rare (~2 Hz) and negligible.
                    let reqs: Vec<_> = self.core.upstream.drain_keyframe_requests(now).collect();
                    for req in reqs {
                        self.core.handle_keyframe_request(req);
                    }
                }
                Some((meta, pkt)) = self.core.downstream.next() => {
                    self.core.handle_forward_rtp(meta, pkt);

                    while let Some(Some((meta, pkt))) = self.core.downstream.next().now_or_never() {
                        self.core.handle_forward_rtp(meta, pkt);
                    }
                },
                Ok(_) = self.udp_egress.writable(), if !self.core.udp_batcher.is_empty() => {
                    // Pure egress flush: no new input to the RTC engine, no need to re-poll.
                    needs_poll = false;
                    self.core.udp_batcher.flush(&self.udp_egress);
                },
                Ok(_) = self.tcp_egress.writable(), if !self.core.tcp_batcher.is_empty() => {
                    // Pure egress flush: no new input to the RTC engine, no need to re-poll.
                    needs_poll = false;
                    self.core.tcp_batcher.flush(&self.tcp_egress);
                },

                // Priority 2: Ingress Work
                Ok(batch) = gateway_rx.recv() => {
                    self.core.handle_udp_packet_batch(batch, now);

                    while let Some(Ok(batch)) = gateway_rx.recv().now_or_never() {
                        self.core.handle_udp_packet_batch(batch, now);
                    }
                },

                // Priority 3: Control Messages
                res = ctx.sys_rx.recv() => {
                    needs_poll = false;
                    match res {
                        Some(msg) => match msg {
                            actor::SystemMsg::GetState(responder) => {
                                let _: () = self.get_observable_state();
                                responder.send(());
                            }
                            actor::SystemMsg::Terminate => break,
                        },
                        None => break,
                    }
                }
                Some(msg) = ctx.rx.recv() => {
                    self.handle_control_message(msg).await;
                }
                res = room_handle.tx.reserve_many(batch_size), if !self.core.events.is_empty() => {
                    needs_poll = false;
                    match res {
                        Ok(permits) => {
                            for (event, permit) in self.core.events.drain(..batch_size).zip(permits) {
                                match event {
                                    CoreEvent::SpawnTrack(rx) => {
                                        permit.send(room::RoomMessage::PublishTrack(rx));
                                    }
                                }
                            }
                        }
                        Err(_e) => {
                            tracing::error!("Room handle closed, shutting down participant actor");
                            break; // Stop the actor so it doesn't spin forever
                        }
                    }
                }


                // Priority 4: Background tasks
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
