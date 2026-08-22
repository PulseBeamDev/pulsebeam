use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    time::Duration,
};

use crate::{
    control::{
        core::{ControllerCore, RoomPlacement},
        negotiator::{Negotiator, NegotiatorError},
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        topology::{TrackAllocation, TrackAllocator, TrackIdentity, TrackSelector, TrackTopology},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId, TrackKind},
    id::ShardId,
    route::{RouteAction, TransportHandle},
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
    topology: TrackTopology,
    track_allocator: TrackAllocator,
    track_keys: HashMap<TrackIdentity, crate::keys::TrackKey>,
    track_allocations: HashMap<(TrackIdentity, ShardId), TrackAllocation>,
    generation: u64,
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
            topology: TrackTopology::default(),
            track_allocator: TrackAllocator::new(shard_count),
            track_keys: HashMap::new(),
            track_allocations: HashMap::new(),
            generation: 0,
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

    async fn handle_shard_event(&mut self, (shard, event): ShardEventMessage) {
        match event {
            ShardEvent::TrackPublished { track } => {
                let Some(identity) = self.topology.publish(*track) else {
                    return;
                };
                self.reconcile_track(identity, false);
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                let Some(room_id) = self
                    .core
                    .registry
                    .get_participant(&origin)
                    .map(|meta| meta.room_id)
                else {
                    debug_assert!(false, "an unpublished track must have a registered origin");
                    return;
                };
                let identity = TrackIdentity {
                    room_id,
                    publisher: origin,
                    id: track_id,
                };
                let members: Vec<_> = self.topology.matches(identity).collect();
                if self.topology.unpublish(identity).is_some() {
                    self.stage_track_removals(identity, members);
                    self.publish_staged();
                    self.release_track_allocations(identity, tokio::time::Instant::now());
                }
            }
            ShardEvent::TrackSubscribed {
                subscriber, track, ..
            } => {
                let identity = TrackIdentity {
                    room_id: track.room_id,
                    publisher: track.origin,
                    id: track.id,
                };
                let _ = self
                    .topology
                    .subscribe(subscriber, TrackSelector::Exact(identity));
                self.reconcile_track(identity, false);
            }
            ShardEvent::TrackUnsubscribed {
                subscriber, track, ..
            } => {
                let identity = TrackIdentity {
                    room_id: track.room_id,
                    publisher: track.origin,
                    id: track.id,
                };
                let _ = self
                    .topology
                    .unsubscribe_matching(subscriber, TrackSelector::Exact(identity));
                self.reconcile_track(identity, false);
            }
            ShardEvent::DataTopicPublished {
                room_id,
                publisher,
                topic,
                ..
            } => self.publish_data_track(
                shard,
                room_id,
                publisher,
                topic,
                crate::track::DataLane::Realtime,
            ),
            ShardEvent::ReliableDataTopicPublished {
                room_id,
                publisher,
                topic,
                ..
            } => self.publish_data_track(
                shard,
                room_id,
                publisher,
                topic,
                crate::track::DataLane::Reliable,
            ),
            ShardEvent::DataTopicSubscribed {
                room_id,
                subscriber,
                publisher,
                topic,
                channel,
                ..
            } => {
                let label =
                    crate::track::publication_label(crate::track::DataLane::Realtime, &topic);
                let selector = TrackSelector::DataTopic {
                    room_id,
                    publisher,
                    label,
                };
                let _ = self.topology.subscribe_data(
                    subscriber,
                    selector,
                    channel,
                    crate::track::DataLane::Realtime,
                );
                let identities: Vec<_> = self
                    .topology
                    .tracks_in_room(room_id, TrackKind::Data)
                    .collect();
                for identity in identities {
                    self.reconcile_track(identity, false);
                }
            }
            ShardEvent::ReliableDataTopicSubscribed {
                room_id,
                subscriber,
                topic,
                channel,
            } => {
                let selector = TrackSelector::DataTopic {
                    room_id,
                    publisher: None,
                    label: crate::track::publication_label(
                        crate::track::DataLane::Reliable,
                        &topic,
                    ),
                };
                let _ = self.topology.subscribe_data(
                    subscriber,
                    selector,
                    channel,
                    crate::track::DataLane::Reliable,
                );
                let identities: Vec<_> = self
                    .topology
                    .tracks_in_room(room_id, TrackKind::Data)
                    .collect();
                for identity in identities {
                    self.reconcile_track(identity, false);
                }
            }
            ShardEvent::DataTopicUnsubscribed {
                room_id,
                subscriber,
                publisher,
                topic,
            } => {
                let selector = TrackSelector::DataTopic {
                    publisher,
                    room_id,
                    label: crate::track::publication_label(
                        crate::track::DataLane::Realtime,
                        &topic,
                    ),
                };
                self.remove_data_subscription(room_id, subscriber, selector);
            }
            ShardEvent::ReliableDataTopicUnsubscribed {
                room_id,
                subscriber,
                topic,
            } => {
                self.remove_data_subscription(
                    room_id,
                    subscriber,
                    TrackSelector::DataTopic {
                        room_id,
                        publisher: None,
                        label: crate::track::publication_label(
                            crate::track::DataLane::Reliable,
                            &topic,
                        ),
                    },
                );
            }
            ShardEvent::DataTopicUnpublished {
                room_id,
                publisher,
                topic,
                ..
            } => self.unpublish_data_track(
                room_id,
                publisher,
                topic,
                crate::track::DataLane::Realtime,
            ),
            ShardEvent::ReliableDataTopicUnpublished {
                room_id,
                publisher,
                topic,
                ..
            } => self.unpublish_data_track(
                room_id,
                publisher,
                topic,
                crate::track::DataLane::Reliable,
            ),
            ShardEvent::TransportAuthenticated {
                source,
                destination,
                shard: owner,
                ..
            } => {
                self.pin_flow_to_owner(source, destination, owner.index() as u16);
                self.emit_placeholder(shard);
            }
            ShardEvent::ParticipantClosed { participant, .. } => {
                self.remove_participant(participant).await;
            }
        }
    }

    fn publish_data_track(
        &mut self,
        shard: ShardId,
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        topic: crate::track::Topic,
        lane: crate::track::DataLane,
    ) {
        let label = crate::track::publication_label(lane, &topic);
        let track = crate::track::Track::data(
            crate::track::TrackMeta {
                room_id,
                shard_id: shard,
                id: publisher.derive_track_id(TrackKind::Data, &label),
                origin: publisher,
            },
            None,
        );
        debug_assert_eq!(
            track.id(),
            publisher.derive_track_id(TrackKind::Data, &label)
        );
        if let Some(identity) = self.topology.publish_data(track, topic, lane) {
            self.reconcile_track(identity, false);
        }
    }

    fn unpublish_data_track(
        &mut self,
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        topic: crate::track::Topic,
        lane: crate::track::DataLane,
    ) {
        let label = crate::track::publication_label(lane, &topic);
        let identity = TrackIdentity {
            room_id,
            publisher,
            id: publisher.derive_track_id(TrackKind::Data, &label),
        };
        let members: Vec<_> = self.topology.matches(identity).collect();
        if self.topology.unpublish(identity).is_some() {
            self.stage_track_removals(identity, members);
            self.publish_staged();
            self.release_track_allocations(identity, tokio::time::Instant::now());
        }
    }

    fn remove_data_subscription(
        &mut self,
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) {
        let Some(meta) = self.core.registry.get_participant(&subscriber) else {
            return;
        };
        let Some(participant) = meta.binding else {
            return;
        };
        let shard = meta.shard_id;
        let identities: Vec<_> = self
            .topology
            .tracks_in_room(room_id, TrackKind::Data)
            .collect();
        let previous_keys: Vec<_> = identities
            .iter()
            .filter_map(|identity| {
                self.track_allocations
                    .get(&(*identity, shard))
                    .map(|allocation| (*identity, allocation.key))
            })
            .collect();
        if self
            .topology
            .remove_matching(subscriber, selector)
            .is_none()
        {
            return;
        }
        for identity in &identities {
            self.reconcile_track(*identity, false);
        }
        let removed: Vec<_> = previous_keys
            .into_iter()
            .filter(|(identity, _)| {
                !self
                    .topology
                    .matches(*identity)
                    .any(|subscription| subscription.subscriber == subscriber)
            })
            .collect();
        if removed.is_empty() {
            return;
        }
        let generation = self.next_generation();
        for (_, key) in removed {
            self.stage_participant_at(
                shard,
                generation,
                participant,
                crate::participant::ParticipantEffect::TrackRemoved(key),
            );
        }
        self.publish_staged();
    }

    fn reconcile_track(&mut self, identity: TrackIdentity, retiring: bool) {
        let now = tokio::time::Instant::now();
        let Some(track) = self.topology.track(identity).cloned() else {
            if retiring {
                self.release_track_allocations(identity, now);
            }
            return;
        };
        let Some(origin_meta) = self.core.registry.get_participant(&identity.publisher) else {
            debug_assert!(false, "a published track must have a live publisher");
            return;
        };
        let origin_shard = origin_meta.shard_id;
        let Some(origin_key) = origin_meta.binding else {
            debug_assert!(false, "a published track must have a publisher key");
            return;
        };
        let mut members: HashMap<ShardId, Vec<(crate::keys::ParticipantKey, ParticipantId)>> =
            HashMap::new();
        for subscription in self.topology.matches(identity) {
            if identity.kind() == TrackKind::Data && subscription.subscriber == identity.publisher {
                continue;
            }
            let Some(meta) = self.core.registry.get_participant(&subscription.subscriber) else {
                continue;
            };
            if meta.room_id != identity.room_id {
                continue;
            }
            let Some(key) = meta.binding else {
                continue;
            };
            members
                .entry(meta.shard_id)
                .or_default()
                .push((key, subscription.subscriber));
        }
        if identity.kind() != TrackKind::Data {
            for participant in self
                .core
                .registry
                .participant_ids_in_room(&identity.room_id)
            {
                if participant == identity.publisher {
                    continue;
                }
                let Some(meta) = self.core.registry.get_participant(&participant) else {
                    continue;
                };
                let Some(key) = meta.binding else {
                    continue;
                };
                members
                    .entry(meta.shard_id)
                    .or_default()
                    .push((key, participant));
            }
        }
        for subscribers in members.values_mut() {
            subscribers.sort_unstable_by_key(|(key, _)| *key);
            subscribers.dedup_by_key(|(key, _)| *key);
        }
        let mut effect_members = members.clone();
        if identity.kind() != TrackKind::Data {
            for participant in self
                .core
                .registry
                .participant_ids_in_room(&identity.room_id)
            {
                if participant == identity.publisher {
                    continue;
                }
                let Some(meta) = self.core.registry.get_participant(&participant) else {
                    continue;
                };
                let Some(key) = meta.binding else {
                    continue;
                };
                effect_members
                    .entry(meta.shard_id)
                    .or_default()
                    .push((key, participant));
            }
        }
        for subscribers in effect_members.values_mut() {
            subscribers.sort_unstable_by_key(|(key, _)| *key);
            subscribers.dedup_by_key(|(key, _)| *key);
        }

        let origin_allocation = match self
            .track_allocations
            .get(&(identity, origin_shard))
            .copied()
        {
            Some(allocation) => allocation,
            None => {
                let Ok(allocation) = self.track_allocator.allocate(origin_shard, identity, now)
                else {
                    debug_assert!(false, "the publisher shard must accept a track allocation");
                    return;
                };
                self.track_allocations
                    .insert((identity, origin_shard), allocation);
                self.track_keys.insert(identity, allocation.key);
                allocation
            }
        };
        if track.kind() == TrackKind::Video && track.reverse().is_none() {
            if let Some(track) = self.topology.track_mut(identity) {
                track.set_reverse(Some(origin_allocation.route));
            }
        }

        let mut desired = effect_members
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        desired.insert(origin_shard);
        let generation = self.next_generation();
        let current: Vec<_> = self
            .track_allocations
            .keys()
            .filter_map(|(held, shard)| (*held == identity).then_some(*shard))
            .collect();
        for destination in current {
            if destination != origin_shard && !desired.contains(&destination) {
                if let Some(allocation) = self.track_allocations.remove(&(identity, destination)) {
                    self.stage_update_at(
                        destination,
                        generation,
                        crate::shard_update::ShardUpdateOp::RetireRoute {
                            handle: allocation.route,
                        },
                    );
                    self.stage_update_at(
                        destination,
                        generation,
                        crate::shard_update::ShardUpdateOp::RemoveTrackRuntime {
                            key: allocation.key,
                        },
                    );
                    self.track_allocator.release(allocation, now);
                }
            }
        }

        let stage_plan = |actor: &mut Self,
                          shard: ShardId,
                          key: crate::keys::TrackKey,
                          local: Vec<crate::keys::ParticipantKey>,
                          remote: Vec<crate::route::RouteHandle>| {
            let reverse = actor
                .topology
                .track(identity)
                .and_then(crate::track::Track::reverse);
            actor.stage_update_at(
                shard,
                generation,
                crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
                    key,
                    runtime: crate::shard_update::TrackRuntime {
                        descriptor: Some(crate::shard_update::TrackDescriptor {
                            id: identity.id,
                            origin_key,
                            participant: (shard == origin_shard).then_some(origin_key),
                            encodings: track.layers().iter().map(|layer| layer.rid).collect(),
                            publication: actor
                                .topology
                                .track(identity)
                                .cloned()
                                .unwrap_or_else(|| track.clone()),
                        }),
                        ..Default::default()
                    },
                },
            );
            actor.stage_update_at(
                shard,
                generation,
                crate::shard_update::ShardUpdateOp::Placeholder,
            );
            actor.stage_plans_at(
                shard,
                generation,
                vec![crate::shard_update::TrackPlanUpdate {
                    key,
                    plan: Some(crate::shard_update::TrackPlan::new(local, remote, reverse)),
                }],
            );
        };

        for destination in effect_members
            .keys()
            .copied()
            .filter(|destination| *destination != origin_shard)
        {
            if !self
                .track_allocations
                .contains_key(&(identity, destination))
            {
                let Ok(allocation) = self.track_allocator.allocate(destination, identity, now)
                else {
                    debug_assert!(false, "an audience shard must accept a track allocation");
                    continue;
                };
                self.track_allocations
                    .insert((identity, destination), allocation);
            }
        }
        let remote_routes: Vec<_> = members
            .keys()
            .filter_map(|destination| {
                (*destination != origin_shard)
                    .then(|| {
                        self.track_allocations
                            .get(&(identity, *destination))
                            .map(|allocation| allocation.route)
                    })
                    .flatten()
            })
            .collect();
        let origin_local = members
            .remove(&origin_shard)
            .unwrap_or_default()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        if track.kind() == TrackKind::Video {
            self.stage_update_at(
                origin_shard,
                generation,
                crate::shard_update::ShardUpdateOp::InstallRoute {
                    binding: crate::shard_update::RouteBinding {
                        handle: origin_allocation.route,
                        action: RouteAction::Reverse {
                            target: origin_allocation.key,
                        },
                    },
                },
            );
        }
        if track.kind() == TrackKind::Data
            && let Some((topic, lane)) = self.topology.data_publication(identity).cloned()
        {
            self.stage_participant_at(
                origin_shard,
                generation,
                origin_key,
                crate::participant::ParticipantEffect::TrackPublished {
                    topic,
                    key: origin_allocation.key,
                    lane,
                },
            );
        }
        stage_plan(
            self,
            origin_shard,
            origin_allocation.key,
            origin_local,
            remote_routes,
        );
        self.stage_participant_at(
            origin_shard,
            generation,
            origin_key,
            crate::participant::ParticipantEffect::TrackSourceBound {
                key: origin_allocation.key,
                track_id: identity.id,
            },
        );
        for destination in effect_members.keys().copied() {
            if destination == origin_shard {
                continue;
            }
            let Some(allocation) = self
                .track_allocations
                .get(&(identity, destination))
                .copied()
            else {
                continue;
            };
            let subscribers = members.remove(&destination).unwrap_or_default();
            self.stage_update_at(
                destination,
                generation,
                crate::shard_update::ShardUpdateOp::InstallRoute {
                    binding: crate::shard_update::RouteBinding {
                        handle: allocation.route,
                        action: RouteAction::Forward {
                            target: allocation.key,
                        },
                    },
                },
            );
            stage_plan(
                self,
                destination,
                allocation.key,
                subscribers.into_iter().map(|(key, _)| key).collect(),
                Vec::new(),
            );
        }
        for (destination, subscribers) in effect_members {
            let key = self
                .track_allocations
                .get(&(identity, destination))
                .map(|allocation| allocation.key)
                .unwrap_or(origin_allocation.key);
            for (_, participant) in subscribers {
                if let Some(meta) = self
                    .core
                    .registry
                    .get_participant(&participant)
                    .and_then(|meta| meta.binding)
                {
                    self.stage_participant_at(
                        destination,
                        generation,
                        meta,
                        crate::participant::ParticipantEffect::TrackInstalled {
                            key,
                            track: track.clone(),
                        },
                    );
                }
            }
        }
        if track.kind() == TrackKind::Data {
            let data_subscriptions: Vec<_> = self
                .topology
                .matches(identity)
                .filter(|subscription| subscription.data.is_some())
                .collect();
            for subscription in data_subscriptions {
                let Some(data) = subscription.data else {
                    continue;
                };
                let Some(meta) = self.core.registry.get_participant(&subscription.subscriber)
                else {
                    continue;
                };
                let Some(participant) = meta.binding else {
                    continue;
                };
                let key = self
                    .track_allocations
                    .get(&(identity, meta.shard_id))
                    .map(|allocation| allocation.key)
                    .unwrap_or(origin_allocation.key);
                self.stage_participant_at(
                    meta.shard_id,
                    generation,
                    participant,
                    crate::participant::ParticipantEffect::TrackSubscribed {
                        key,
                        channel: data.channel,
                        lane: data.lane,
                    },
                );
            }
        }
        self.publish_staged();
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        debug_assert_ne!(self.generation, 0);
        self.generation
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
            .filter(|participant| Some(*participant) != removed)
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

    fn release_track_allocations(&mut self, identity: TrackIdentity, now: tokio::time::Instant) {
        let allocations: Vec<_> = self
            .track_allocations
            .extract_if(|(held, _), _| *held == identity)
            .map(|(_, allocation)| allocation)
            .collect();
        for allocation in allocations {
            self.track_allocator.release(allocation, now);
        }
        self.track_keys.remove(&identity);
    }

    fn stage_track_removals(
        &mut self,
        identity: TrackIdentity,
        members: Vec<crate::control::topology::Subscription>,
    ) {
        let generation = self.next_generation();
        self.stage_track_removals_at(identity, members, generation);
    }

    fn stage_track_removals_at(
        &mut self,
        identity: TrackIdentity,
        members: Vec<crate::control::topology::Subscription>,
        generation: u64,
    ) {
        let mut removed = HashSet::new();
        for member in members {
            if identity.kind() == TrackKind::Data && member.subscriber == identity.publisher {
                continue;
            }
            let Some(meta) = self.core.registry.get_participant(&member.subscriber) else {
                continue;
            };
            let Some(key) = meta.binding else { continue };
            if !removed.insert(member.subscriber) {
                continue;
            }
            let track_key = self
                .track_allocations
                .get(&(identity, meta.shard_id))
                .map(|allocation| allocation.key)
                .or_else(|| self.track_keys.get(&identity).copied())
                .unwrap_or_default();
            self.stage_participant_at(
                meta.shard_id,
                generation,
                key,
                crate::participant::ParticipantEffect::TrackRemoved(track_key),
            );
        }
        if identity.kind() != TrackKind::Data {
            for participant in self
                .core
                .registry
                .participant_ids_in_room(&identity.room_id)
            {
                if !removed.insert(participant) {
                    continue;
                }
                let Some(meta) = self.core.registry.get_participant(&participant) else {
                    continue;
                };
                let Some(key) = meta.binding else {
                    continue;
                };
                let track_key = self
                    .track_allocations
                    .get(&(identity, meta.shard_id))
                    .map(|allocation| allocation.key)
                    .or_else(|| self.track_keys.get(&identity).copied())
                    .unwrap_or_default();
                self.stage_participant_at(
                    meta.shard_id,
                    generation,
                    key,
                    crate::participant::ParticipantEffect::TrackRemoved(track_key),
                );
            }
        }
        let allocations: Vec<_> = self
            .track_allocations
            .iter()
            .filter_map(|((held, shard), allocation)| {
                (*held == identity).then_some((*shard, *allocation))
            })
            .collect();
        for (shard, allocation) in allocations {
            self.stage_update_at(
                shard,
                generation,
                crate::shard_update::ShardUpdateOp::RetireRoute {
                    handle: allocation.route,
                },
            );
            self.stage_update_at(
                shard,
                generation,
                crate::shard_update::ShardUpdateOp::RemoveTrackRuntime {
                    key: allocation.key,
                },
            );
            self.stage_plans_at(
                shard,
                generation,
                vec![crate::shard_update::TrackPlanUpdate {
                    key: allocation.key,
                    plan: None,
                }],
            );
        }
    }

    fn emit_placeholder(&mut self, shard: ShardId) {
        self.generation = self.generation.saturating_add(1);
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "shard update targeted an unknown shard");
            return;
        };
        update.stage(
            self.generation,
            crate::shard_update::ShardUpdateOp::Placeholder,
        );
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
        self.generation = self.generation.saturating_add(1);
        let Some(update) = self.updates.get_mut(shard.index()) else {
            return false;
        };
        update.stage(
            self.generation,
            crate::shard_update::ShardUpdateOp::InsertParticipant,
        );
        update.stage(
            self.generation,
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
            RoomPlacement::RoundRobin => ShardId::new(slot % self.router.shard_count()),
        };
        let now = tokio::time::Instant::now();
        let handle = self
            .core
            .reserve_transport(shard, now)
            .map_err(|_| ControllerError::ServiceUnavailable)?;
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
        let generation = self.next_generation();
        self.stage_participant_change_at(room_id, generation, Some(participant_id), None);
        if let Some(meta) = self.core.registry.get_participant(&participant_id)
            && let Some(key) = meta.binding
        {
            let participants = self.core.registry.participant_ids_in_room(&room_id);
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
        let room_tracks: Vec<_> = self
            .topology
            .identities()
            .filter(|identity| identity.room_id == room_id)
            .collect();
        for identity in room_tracks {
            self.reconcile_track(identity, false);
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
        self.remove_participant(state.participant_id).await;
        self.handle_create_participant(state, offer).await
    }

    async fn remove_participant(&mut self, participant: ParticipantId) {
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
        let affected: Vec<_> = self
            .topology
            .identities()
            .filter(|identity| {
                identity.publisher == participant
                    || self
                        .topology
                        .matches(*identity)
                        .any(|subscription| subscription.subscriber == participant)
                    || (identity.kind() != TrackKind::Data
                        && self
                            .core
                            .registry
                            .get_participant(&participant)
                            .is_some_and(|meta| meta.room_id == identity.room_id))
            })
            .collect();
        let retiring: Vec<_> = affected
            .iter()
            .copied()
            .filter(|identity| identity.publisher == participant)
            .collect();
        let generation = self.next_generation();
        for identity in retiring.iter().copied() {
            let members: Vec<_> = self.topology.matches(identity).collect();
            self.stage_track_removals_at(identity, members, generation);
            let _ = self.topology.unpublish(identity);
        }
        let room_id = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| meta.room_id);
        if let Some(room_id) = room_id {
            self.stage_participant_change_at(room_id, generation, None, Some(participant));
        }
        let _ = self.core.delete_participant(&participant);
        self.topology.remove_participant(participant);
        self.publish_staged();
        let now = tokio::time::Instant::now();
        for identity in retiring {
            self.release_track_allocations(identity, now);
        }
        for identity in affected {
            if !self.topology.contains(identity) {
                continue;
            }
            self.reconcile_track(identity, false);
        }
        let Some(handle) = meta.transport else { return };
        let generation = self.next_generation();
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
