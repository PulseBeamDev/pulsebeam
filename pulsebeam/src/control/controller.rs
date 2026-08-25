use std::{collections::VecDeque, io, time::Duration};

use crate::track::{SelectionPolicy, TrackSelector};
use crate::{
    control::{
        core::{ControllerCore, RoomPlacement},
        lifecycle::{TrackLifecycle, TrackLifecycleOperation, TrackLifecycleOutcome},
        negotiator::{Negotiator, NegotiatorError},
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    route::TransportHandle,
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_runtime::mailbox;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

#[derive(Debug, derive_more::From)]
pub enum ControllerCommand {
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

#[derive(Debug)]
pub struct CreateParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] NegotiatorError),
    #[error("server is busy, please try again later.")]
    ServiceUnavailable,
    #[error("participant connection generation is stale")]
    StaleConnection,
    #[error("IO error: {0}")]
    IOError(#[from] io::Error),
    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct ControllerActor {
    router: crate::control::router::ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    tcp_listener: Option<pulsebeam_core::net::TcpListener>,
    cluster_id: u16,
    node_id: u16,
    updates: Vec<crate::shard_update::ShardUpdateWriter>,
    lifecycle: TrackLifecycle,
    command_backlog: VecDeque<(ShardId, ShardCommand)>,
    #[cfg(not(feature = "sim"))]
    steering: Option<crate::ebpf::Steering>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<Candidate>,
        tcp_listener: pulsebeam_core::net::TcpListener,
        room_shard_slot: usize,
        placement: RoomPlacement,
        updates: Vec<crate::shard_update::ShardUpdateWriter>,
    ) -> Self {
        let shard_count = shard_contexts.len();
        debug_assert_eq!(updates.len(), shard_count);
        Self {
            router: crate::control::router::ShardRouter::new(shard_contexts),
            core: ControllerCore::with_shards(shard_count, room_shard_slot, placement),
            negotiator: Negotiator::new(candidates),
            tcp_listener: Some(tcp_listener),
            cluster_id: 0,
            node_id: 0,
            updates,
            lifecycle: TrackLifecycle::new(shard_count),
            command_backlog: VecDeque::new(),
            #[cfg(not(feature = "sim"))]
            steering: None,
        }
    }

    #[cfg(not(feature = "sim"))]
    pub(crate) fn set_steering(&mut self, steering: crate::ebpf::Steering) {
        debug_assert!(self.steering.is_none());
        self.steering = Some(steering);
    }

    #[cfg(not(feature = "sim"))]
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        let Some(steering) = self.steering.as_mut() else {
            return;
        };
        let flow = crate::ebpf::flow_key(source, destination);
        if let Err(error) = steering.install_flow(flow, shard) {
            tracing::warn!(%error, shard, "failed to install authenticated eBPF flow");
        }
    }

    #[cfg(feature = "sim")]
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        pulsebeam_runtime::net::install_steering_flow(source, destination, shard);
    }

    pub(crate) async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEventMessage>,
        shutdown: CancellationToken,
    ) {
        let Some(listener) = self.tcp_listener.take() else {
            pulsebeam_runtime::fatal!("ControllerActor::run called twice")
        };
        let acceptor = TcpAcceptorHandle::spawn(
            listener,
            crate::control::tcp_acceptor::TcpAcceptorConfig {
                cluster_id: self.cluster_id,
                node_id: self.node_id,
                shard_count: self.router.shard_count(),
            },
            shutdown.child_token(),
        );
        let mut pending_rx = acceptor.event_rx;
        let mut poll_interval = tokio::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                Some(event) = shard_event_rx.recv() => self.handle_shard_event(event).await,
                _ = self.core.next_expired() => {},
                _ = poll_interval.tick() => {
                    self.router.poll_loads();
                    self.flush_command_backlog();
                    for update in &mut self.updates { let _ = update.flush_backlog(); }
                }
                Some(event) = pending_rx.recv() => {
                    if let Some(connection) = event.result { self.route_tcp_connection(connection); }
                }
                Some(command) = command_rx.recv() => self.process_command(command).await,
                _ = shutdown.cancelled() => break,
                else => break,
            }
            self.flush_command_backlog();
        }
    }

    pub async fn process_command(&mut self, command: ControllerCommand) {
        match command {
            ControllerCommand::CreateParticipant(message, reply) => {
                let result = self
                    .handle_create_participant(message.state, message.offer)
                    .await
                    .map(|answer| CreateParticipantReply { answer });
                let _ = reply.send(result);
            }
            ControllerCommand::DeleteParticipant(message) => {
                self.remove_participant(message.participant_id).await;
            }
            ControllerCommand::PatchParticipant(message, reply) => {
                let result = self
                    .handle_patch_participant(message.state, message.offer)
                    .await
                    .map(|answer| PatchParticipantReply { answer });
                let _ = reply.send(result);
            }
        }
    }

    async fn handle_shard_event(&mut self, (_shard, event): ShardEventMessage) {
        match event {
            ShardEvent::TrackPublished { track } => {
                if let Some(outcome) =
                    self.lifecycle
                        .publish(track, &self.core.registry, tokio::time::Instant::now())
                {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                if let Some(outcome) = self.lifecycle.unpublish(
                    origin,
                    track_id,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                ) {
                    self.apply_track_lifecycle(outcome);
                    self.publish_staged();
                }
            }
            ShardEvent::TrackSubscribed {
                subscriber, track, ..
            } => {
                let outcome = self.lifecycle.activate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackUnsubscribed {
                subscriber, track, ..
            } => {
                let outcome = self.lifecycle.deactivate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackSubscriptionAdded {
                room_id,
                subscriber,
                selector,
                selection,
            } => {
                let outcomes = self.lifecycle.subscribe(
                    room_id,
                    subscriber,
                    selector,
                    selection,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TrackSubscriptionRemoved {
                room_id,
                subscriber,
                selector,
            } => {
                let outcomes = self.lifecycle.unsubscribe(
                    room_id,
                    subscriber,
                    selector,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TransportAuthenticated {
                source,
                destination,
                source_shard,
                handle,
                shard: owner_shard,
                ..
            } => {
                let Some(owner) = u16::try_from(owner_shard.index()).ok() else {
                    debug_assert!(false, "shard index must fit in u16");
                    return;
                };
                self.pin_flow_to_owner(source, destination, owner);
                self.command_backlog.push_back((
                    source_shard,
                    ShardCommand::AuthenticateTransport { source, handle },
                ));
                self.emit_placeholder(owner_shard);
            }
            ShardEvent::ParticipantClosed { participant, .. } => {
                self.remove_participant_with_mode(participant, true).await;
            }
        }
    }
}

impl ControllerActor {
    fn apply_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        for operation in outcome.operations {
            match operation {
                TrackLifecycleOperation::Update { shard, op } => {
                    self.stage_update_at(shard, outcome.generation, op);
                }
                TrackLifecycleOperation::Plans { shard, plans } => {
                    self.stage_plans_at(shard, outcome.generation, plans);
                }
                TrackLifecycleOperation::ParticipantEffect {
                    shard,
                    participant,
                    effect,
                } => self.stage_participant_at(shard, outcome.generation, participant, effect),
            }
        }
    }

    fn publish_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        self.apply_track_lifecycle(outcome);
        self.publish_staged();
    }

    fn stage_update_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        op: crate::shard_update::ShardUpdateOp,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "an update must target a live shard");
            return;
        };
        update.stage(generation, op);
    }

    fn stage_plans_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        plans: Vec<crate::shard_update::TrackPlanUpdate>,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "plans must target a live shard");
            return;
        };
        update.stage_plans(generation, plans);
    }

    fn stage_participant_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        participant: crate::keys::ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "participant effects must target a live shard");
            return;
        };
        update.stage_participant_effect(generation, participant, effect);
    }

    fn stage_participant_change_at(
        &mut self,
        room_id: crate::entity::RoomId,
        generation: u64,
        added: Option<ParticipantId>,
        removed: Option<ParticipantId>,
    ) {
        let participants: Vec<_> = self
            .core
            .registry
            .participant_ids_in_room(&room_id)
            .into_iter()
            .filter(|participant| Some(*participant) != added && Some(*participant) != removed)
            .collect();
        for participant in participants {
            let Some(meta) = self.core.registry.get_participant(&participant) else {
                continue;
            };
            let Some(key) = meta.binding else {
                continue;
            };
            self.stage_participant_at(
                meta.shard_id,
                generation,
                key,
                crate::participant::ParticipantEffect::ParticipantsChanged {
                    added: added.into_iter().collect(),
                    removed: removed.into_iter().collect(),
                },
            );
        }
    }

    fn publish_staged(&mut self) {
        for update in &mut self.updates {
            let _ = update.publish();
        }
    }

    fn emit_placeholder(&mut self, shard: ShardId) {
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "shard update targeted an unknown shard");
            return;
        };
        update.stage(generation, crate::shard_update::ShardUpdateOp::Placeholder);
        let _ = update.publish();
    }

    fn flush_command_backlog(&mut self) {
        while let Some((shard, command)) = self.command_backlog.pop_front() {
            match self.router.try_send(shard, command) {
                Ok(()) => {}
                Err(error) => match *error {
                    mailbox::TrySendError::Full(command) => {
                        self.command_backlog.push_front((shard, command));
                        break;
                    }
                    mailbox::TrySendError::Closed(_) => {
                        tracing::warn!(%shard, "shard command mailbox closed");
                    }
                },
            }
        }
    }

    fn route_tcp_connection(&mut self, connection: PendingTcpConn) {
        debug_assert!(connection.handle.shard().index() < self.router.shard_count());
        self.command_backlog.push_back((
            connection.handle.shard(),
            ShardCommand::AdoptTcpConnection {
                stream: connection.stream,
                peer_addr: connection.peer_addr,
            },
        ));
    }

    fn publish_transport(
        &mut self,
        shard: ShardId,
        handle: TransportHandle,
        key: crate::keys::ParticipantKey,
    ) -> bool {
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            return false;
        };
        update.stage(
            generation,
            crate::shard_update::ShardUpdateOp::InsertParticipant,
        );
        update.stage(
            generation,
            crate::shard_update::ShardUpdateOp::InstallTransport {
                binding: crate::shard_update::TransportBinding {
                    handle,
                    participant: key,
                },
            },
        );
        update.publish().is_some()
    }

    async fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let participant_id = state.participant_id;
        let (slot, placement) = self.core.room_slot(&state.room_id);
        let shard = match placement {
            RoomPlacement::Hashed => self
                .router
                .stable_route(&state.room_id)
                .ok_or(ControllerError::ServiceUnavailable)?,
            RoomPlacement::RoundRobin => {
                let shard_count = self.router.shard_count();
                debug_assert_ne!(shard_count, 0);
                ShardId::new(
                    slot.checked_rem(shard_count)
                        .ok_or(ControllerError::ServiceUnavailable)?,
                )
            }
        };
        let now = tokio::time::Instant::now();
        let handle = self.core.reserve_transport(shard, now);
        let key = self
            .core
            .mint_participant(shard, state.participant_id)
            .ok_or(ControllerError::ServiceUnavailable)?;
        let creds = IceUfrag::new(self.cluster_id, self.node_id, handle.route, handle.epoch)
            .into_ice_creds();
        let (rtc, answer) = match self.negotiator.create_answer(offer, creds) {
            Ok(value) => value,
            Err(error) => {
                self.core.remove_participant_key(shard, key);
                self.core.release_transport(handle, now);
                return Err(error.into());
            }
        };
        if !self.publish_transport(shard, handle, key) {
            self.core.remove_participant_key(shard, key);
            self.core.release_transport(handle, now);
            return Err(ControllerError::ServiceUnavailable);
        }
        let config = self.core.create_participant(rtc, state, shard, handle, key);
        let room_id = config.room_id;
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .router
            .send(
                shard,
                ShardCommand::MaterializeParticipant {
                    key,
                    transport: handle,
                    config: Box::new(config),
                    ack: ack_tx,
                },
            )
            .await
            .is_err()
        {
            self.remove_participant(participant_id).await;
            return Err(ControllerError::ServiceUnavailable);
        }
        if !ack_rx.await.unwrap_or(false) {
            self.remove_participant(participant_id).await;
            return Err(ControllerError::ServiceUnavailable);
        }
        let generation = self.lifecycle.next_generation();
        self.stage_participant_change_at(room_id, generation, Some(participant_id), None);
        if let Some(meta) = self.core.registry.get_participant(&participant_id)
            && let Some(key) = meta.binding
        {
            let participants = self
                .core
                .registry
                .participant_ids_in_room(&room_id)
                .into_iter()
                .filter(|participant| *participant != participant_id)
                .collect();
            self.stage_participant_at(
                meta.shard_id,
                generation,
                key,
                crate::participant::ParticipantEffect::ParticipantsChanged {
                    added: participants,
                    removed: Vec::new(),
                },
            );
        }
        self.publish_staged();
        let outcomes = self.lifecycle.subscribe_defaults(
            room_id,
            participant_id,
            [
                (TrackSelector::audio(), SelectionPolicy::All),
                (TrackSelector::video(), SelectionPolicy::Allocated),
            ],
            &self.core.registry,
            tokio::time::Instant::now(),
        );
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        Ok(answer)
    }

    async fn handle_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let current = self
            .core
            .registry
            .get_participant(&state.participant_id)
            .and_then(|meta| meta.connection_id);
        if current != state.old_connection_id {
            return Err(ControllerError::StaleConnection);
        }
        self.remove_participant_with_mode(state.participant_id, true)
            .await;
        self.handle_create_participant(state, offer).await
    }

    async fn remove_participant(&mut self, participant: ParticipantId) {
        self.remove_participant_with_mode(participant, false).await;
    }

    async fn remove_participant_with_mode(
        &mut self,
        participant: ParticipantId,
        retain_identity: bool,
    ) {
        let Some(meta) = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| crate::control::core::ParticipantMeta {
                shard: meta.shard_id,
                binding: meta.binding,
                transport: meta.transport,
            })
        else {
            return;
        };
        let mut outcomes = self.lifecycle.remove_participant(
            participant,
            &self.core.registry,
            tokio::time::Instant::now(),
        );
        let generation = outcomes
            .first()
            .map(|outcome| outcome.generation)
            .unwrap_or_else(|| self.lifecycle.next_generation());
        if let Some(outcome) = outcomes.first()
            && outcome.generation == generation
        {
            let outcome = outcomes.remove(0);
            self.apply_track_lifecycle(outcome);
        }
        let room_id = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| meta.room_id);
        if let Some(room_id) = room_id {
            self.stage_participant_change_at(room_id, generation, None, Some(participant));
        }
        if retain_identity {
            let _ = self.core.disconnect_participant(&participant);
        } else {
            let _ = self.core.delete_participant(&participant);
        }
        self.publish_staged();
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        let Some(handle) = meta.transport else { return };
        let generation = self.lifecycle.next_generation();
        if let Some(update) = self.updates.get_mut(meta.shard.index()) {
            update.stage(
                generation,
                crate::shard_update::ShardUpdateOp::RetireTransport { handle },
            );
            if let Some(key) = meta.binding {
                update.stage(
                    generation,
                    crate::shard_update::ShardUpdateOp::RemoveParticipant { key },
                );
            }
            let _ = update.publish();
        }
        if let Some(key) = meta.binding {
            self.core.remove_participant_key(meta.shard, key);
        }
        self.core
            .release_transport(handle, tokio::time::Instant::now());
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;
