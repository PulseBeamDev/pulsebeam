use std::{
    collections::BTreeMap,
    future::pending,
    num::NonZeroU64,
    time::{Duration, SystemTime},
};

use pulsebeam_core::{auth::VerifiedPrincipal, identity::NodeId};

use pulsebeam_runtime::{sync::oneshot, task, time::Instant};

use super::{
    ConnectionEnd, ControllerActor, ControllerError, EndpointKey, NativeResourceRequest,
    PublisherHandoffCommand,
    ownership::{
        ClaimRank, Incarnation,
        observations::Session,
        receipt::{Authority, Error, Issuer, PeerTrust},
    },
};
use crate::{
    cluster::handoff::{Fences, Handoff, Owner, PendingDelivery, Scope, Store},
    entity::{ConnectionId, ProjectRoomKey, RoomId},
};

fn scope(endpoint: EndpointKey) -> Scope {
    Scope {
        project: endpoint.room.project,
        room: endpoint.room.room,
        participant: endpoint.participant,
    }
}

fn endpoint(scope: Scope) -> EndpointKey {
    EndpointKey {
        room: ProjectRoomKey {
            project: scope.project,
            room: scope.room,
        },
        participant: scope.participant,
    }
}

fn owner(issuer: Issuer, connection: ConnectionId) -> Owner {
    Owner {
        domain: issuer.trust_domain,
        node: issuer.node,
        generation: issuer.incarnation,
        connection,
    }
}

const HANDOFF_DOMAIN: &[u8] = b"pulsebeam.publisher.handoff.v1\0";
const FRAME_TAG: &[u8] = &[1, 0x0f];
const RETRY: Duration = Duration::from_millis(150);
const TARGET_CAPACITY: usize = 128;
type Origin = (NodeId, NonZeroU64);

pub(super) enum Frame {
    Offer(Box<Proof>),
    Committed([u8; 64]),
}

impl Frame {
    pub(super) fn encode(self) -> Result<Box<[u8]>, Error> {
        let mut bytes = FRAME_TAG.to_vec();
        match self {
            Self::Offer(proof) => {
                bytes.push(0);
                bytes.extend_from_slice(&proof.signature);
                serde_json::to_writer(&mut bytes, &proof.handoff).map_err(|_| Error::Invalid)?;
            }
            Self::Committed(signature) => {
                bytes.push(1);
                bytes.extend_from_slice(&signature);
            }
        }
        if bytes.len() > pulsebeam_link::CONTROL_FRAME_BYTES {
            return Err(Error::Invalid);
        }
        Ok(bytes.into_boxed_slice())
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > pulsebeam_link::CONTROL_FRAME_BYTES {
            return None;
        }
        let (&kind, body) = bytes.strip_prefix(FRAME_TAG)?.split_first()?;
        let (signature, body) = body.split_first_chunk::<64>()?;
        match kind {
            0 => Some(Self::Offer(Box::new(Proof {
                signature: *signature,
                handoff: serde_json::from_slice(body).ok()?,
            }))),
            1 if body.is_empty() => Some(Self::Committed(*signature)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Proof {
    pub(super) handoff: Handoff,
    signature: [u8; 64],
}

fn issuer(owner: Owner) -> Issuer {
    Issuer {
        trust_domain: owner.domain,
        node: owner.node,
        incarnation: owner.generation,
    }
}

impl Proof {
    fn message(handoff: Handoff) -> Result<Vec<u8>, Error> {
        let mut message = HANDOFF_DOMAIN.to_vec();
        serde_json::to_writer(&mut message, &handoff).map_err(|_| Error::Invalid)?;
        Ok(message)
    }

    fn issue(trust: &impl PeerTrust, handoff: Handoff) -> Result<Self, Error> {
        if issuer(handoff.predecessor) != trust.issuer() {
            return Err(Error::NoAuthority);
        }
        let signature = trust
            .sign(&Self::message(handoff)?)
            .ok_or(Error::NoAuthority)?;
        Ok(Self { handoff, signature })
    }

    fn verify(self, trust: &impl PeerTrust, origin: Origin) -> Result<(), Error> {
        let predecessor = issuer(self.handoff.predecessor);
        let successor = issuer(self.handoff.successor);
        if (predecessor.node, predecessor.incarnation) != origin
            || predecessor.node == successor.node
        {
            return Err(Error::Scope);
        }
        if trust.eligible_key(successor).is_none() {
            return Err(Error::NoAuthority);
        }
        let key = trust.eligible_key(predecessor).ok_or(Error::NoAuthority)?;
        key.verify_message(&Self::message(self.handoff)?, &self.signature)
            .then_some(())
            .ok_or(Error::Invalid)
    }
}

type CommandReply = oneshot::Sender<Result<(), ControllerError>>;

pub(super) enum Request {
    Start {
        resource: NativeResourceRequest,
        successor: Owner,
        sequence: NonZeroU64,
        reply: CommandReply,
    },
    Prepare {
        principal: VerifiedPrincipal,
        room: RoomId,
        reply: oneshot::Sender<Result<Owner, ControllerError>>,
    },
    Inspect {
        resource: NativeResourceRequest,
        room: RoomId,
        successor: Owner,
        sequence: NonZeroU64,
        reply: oneshot::Sender<Result<Status, ControllerError>>,
    },
}

pub(super) enum Status {
    Pending,
    Completed,
    NotFound,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublisherHandoffRequest([redacted])")
    }
}

impl PublisherHandoffCommand {
    /// Internal controller boundary. Success means every pinned peer persisted
    /// the fence and the local acknowledgement checkpoint is durable.
    pub(crate) fn new(
        resource: NativeResourceRequest,
        successor: Owner,
        sequence: NonZeroU64,
        reply: CommandReply,
    ) -> Self {
        Self(Box::new(Request::Start {
            resource,
            successor,
            sequence,
            reply,
        }))
    }

    pub(super) fn prepare(
        principal: VerifiedPrincipal,
        room: RoomId,
        reply: oneshot::Sender<Result<Owner, ControllerError>>,
    ) -> Self {
        Self(Box::new(Request::Prepare {
            principal,
            room,
            reply,
        }))
    }

    pub(super) fn inspect(
        resource: NativeResourceRequest,
        room: RoomId,
        successor: Owner,
        sequence: NonZeroU64,
        reply: oneshot::Sender<Result<Status, ControllerError>>,
    ) -> Self {
        Self(Box::new(Request::Inspect {
            resource,
            room,
            successor,
            sequence,
            reply,
        }))
    }
}

type Reply = oneshot::Sender<Result<Proof, Error>>;
type Receipt = oneshot::Receiver<Result<Proof, Error>>;
type WriteResult = (Store, anyhow::Result<Fences>);
pub(super) enum Completion {
    Handoff(Result<WriteResult, task::JoinError>),
    Acknowledgement(Result<(Store, anyhow::Result<()>), task::JoinError>),
}

struct Transfer {
    endpoint: EndpointKey,
    predecessor: Option<(Incarnation, ClaimRank)>,
    proof: Proof,
    peers: Box<[Origin]>,
    reply: Option<Reply>,
    command_reply: Option<CommandReply>,
    write: Option<task::JoinHandle<WriteResult>>,
}

struct Delivery {
    endpoint: EndpointKey,
    rank: ClaimRank,
    proof: Proof,
    outstanding: BTreeMap<Origin, Option<Instant>>,
    reply: Option<CommandReply>,
}

#[derive(Default)]
pub(super) struct PublisherHandoffs {
    fences: Fences,
    // Once returned to the operator, a reservation must survive delayed delivery
    // of the offer: the predecessor may already have retired durably.
    targets: BTreeMap<Scope, Owner>,
    store: Option<Store>,
    transfer: Option<Transfer>,
    delivery: Option<Delivery>,
    ack_write: Option<task::JoinHandle<(Store, anyhow::Result<()>)>>,
}

impl PublisherHandoffs {
    pub(super) fn set_store(&mut self, store: Store) -> anyhow::Result<()> {
        if let Some(pending) = store.pending() {
            let Some(Frame::Offer(proof)) = Frame::decode(&pending.offer) else {
                anyhow::bail!("invalid pending publisher handoff frame");
            };
            anyhow::ensure!(
                store.current(pending.scope)? == Some(proof.handoff),
                "pending publisher handoff does not match its fence"
            );
            let rank = ClaimRank::from_persisted(
                pending.lineage,
                pending.position,
                proof.handoff.predecessor.connection,
            )
            .ok_or_else(|| anyhow::anyhow!("invalid retiring publisher claim"))?;
            let outstanding: BTreeMap<_, _> =
                pending.peers.iter().map(|&peer| (peer, None)).collect();
            let successor = proof.handoff.successor;
            anyhow::ensure!(
                outstanding.len() == pending.peers.len()
                    && outstanding.len() <= crate::link::host::PEERS
                    && outstanding.contains_key(&(successor.node, successor.generation))
                    && outstanding
                        .keys()
                        .all(|(node, _)| *node != proof.handoff.predecessor.node),
                "invalid pending publisher handoff peers"
            );
            self.delivery = Some(Delivery {
                endpoint: endpoint(pending.scope),
                rank,
                proof: *proof,
                outstanding,
                reply: None,
            });
        }
        self.fences = store.fences();
        self.store = Some(store);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn restore(&mut self, fences: Fences) {
        self.fences = fences;
    }

    fn prepare_target(&mut self, scope: Scope, issuer: Issuer) -> Result<Owner, ControllerError> {
        if let Some(&target) = self.targets.get(&scope) {
            return Ok(target);
        }
        if self.targets.len() >= TARGET_CAPACITY {
            return Err(ControllerError::ServiceUnavailable);
        }
        let target = owner(issuer, ConnectionId::new());
        self.targets.insert(scope, target);
        Ok(target)
    }

    pub(super) fn local_target(
        &self,
        endpoint: EndpointKey,
        issuer: Issuer,
    ) -> Option<ConnectionId> {
        let target = self.fences.successor(scope(endpoint))?;
        (issuer == self::issuer(target)).then_some(target.connection)
    }

    pub(super) fn reserved_connection(
        &self,
        endpoint: EndpointKey,
        issuer: Issuer,
    ) -> Option<ConnectionId> {
        let connection = self.local_target(endpoint, issuer)?;
        let prepared = self.targets.get(&scope(endpoint))?;
        (*prepared == owner(issuer, connection)).then_some(connection)
    }

    pub(super) fn consume_target(&mut self, endpoint: EndpointKey) {
        self.targets.remove(&scope(endpoint));
    }

    pub(super) fn permits(
        &self,
        endpoint: EndpointKey,
        issuer: Issuer,
        connection: ConnectionId,
    ) -> bool {
        self.transfer
            .as_ref()
            .is_none_or(|transfer| transfer.endpoint != endpoint)
            && self
                .fences
                .allows(scope(endpoint), owner(issuer, connection))
    }

    pub(super) fn retiring_claim(&self) -> Option<(EndpointKey, ClaimRank)> {
        self.transfer
            .as_ref()
            .and_then(|transfer| {
                transfer
                    .predecessor
                    .map(|(_, rank)| (transfer.endpoint, rank))
            })
            .or_else(|| {
                self.delivery
                    .as_ref()
                    .map(|delivery| (delivery.endpoint, delivery.rank))
            })
    }

    pub(super) fn offer(&mut self, remote: Origin, now: Instant) -> Option<Box<[u8]>> {
        if self.ack_write.is_some() {
            return None;
        }
        let delivery = self.delivery.as_mut()?;
        let retry_at = delivery.outstanding.get_mut(&remote)?;
        if retry_at.is_some_and(|deadline| now < deadline) {
            return None;
        }
        *retry_at = now.checked_add(RETRY);
        Frame::Offer(Box::new(delivery.proof)).encode().ok()
    }

    fn acknowledge(&mut self, remote: Origin, signature: [u8; 64]) {
        let Some(delivery) = self.delivery.as_mut().filter(|delivery| {
            delivery.proof.signature == signature && delivery.outstanding.contains_key(&remote)
        }) else {
            return;
        };
        if delivery.outstanding.len() > 1 {
            delivery.outstanding.remove(&remote);
            return;
        }
        // The last acknowledgement releases the retiring claim only after the
        // checkpoint clears the outbox. If the store is busy, retry this peer.
        let Some(mut store) = self.store.take() else {
            return;
        };
        let scope = delivery.proof.handoff.scope;
        self.ack_write = Some(task::spawn_blocking(move || {
            let result = store.confirm_delivery(scope);
            (store, result)
        }));
    }

    pub(super) fn cutoff_applied(&mut self, incarnation: Incarnation) {
        if self.transfer.as_ref().is_some_and(|transfer| {
            transfer
                .predecessor
                .is_some_and(|(retiring, _)| retiring == incarnation)
        }) {
            self.start_write();
        }
    }

    fn start_write(&mut self) {
        let Some(transfer) = self
            .transfer
            .as_mut()
            .filter(|transfer| transfer.write.is_none() && transfer.reply.is_some())
        else {
            return;
        };
        let Some(mut store) = self.store.take() else {
            return;
        };
        let proof = transfer.proof;
        let rank = transfer.predecessor.map(|(_, rank)| rank);
        let peers = transfer.peers.clone();
        transfer.write = Some(task::spawn_blocking(move || {
            let handoff = proof.handoff;
            let result = if let Some(rank) = rank {
                Frame::Offer(Box::new(proof))
                    .encode()
                    .map_err(|_| anyhow::anyhow!("encoding pending publisher handoff"))
                    .and_then(|offer| {
                        store.commit_outgoing(
                            handoff,
                            PendingDelivery {
                                scope: handoff.scope,
                                lineage: rank.lineage(),
                                position: rank.position(),
                                offer,
                                peers,
                            },
                        )
                    })
            } else {
                store.commit(handoff)
            }
            .map(|()| store.fences());
            (store, result)
        }));
    }

    pub(super) async fn wait_for_write(&mut self) -> Completion {
        if let Some(write) = self.ack_write.as_mut() {
            return Completion::Acknowledgement(write.await);
        }
        match self
            .transfer
            .as_mut()
            .and_then(|transfer| transfer.write.as_mut())
        {
            Some(write) => Completion::Handoff(write.await),
            None => pending().await,
        }
    }

    pub(super) fn complete_write(&mut self, completion: Completion) {
        let completion = match completion {
            Completion::Handoff(result) => result,
            Completion::Acknowledgement(result) => {
                self.ack_write = None;
                if let Ok((store, Ok(()))) = result {
                    self.store = Some(store);
                    if let Some(reply) = self.delivery.take().and_then(|delivery| delivery.reply) {
                        let _ = reply.send(Ok(()));
                    }
                } else if let Some(reply) = self
                    .delivery
                    .as_mut()
                    .and_then(|delivery| delivery.reply.take())
                {
                    let _ = reply.send(Err(ControllerError::ServiceUnavailable));
                }
                return;
            }
        };
        let Some(transfer) = self.transfer.as_mut() else {
            return;
        };
        transfer.write = None;
        let reply = transfer.reply.take();
        match completion {
            Ok((store, Ok(fences))) => {
                self.store = Some(store);
                self.fences = fences;
                let proof = transfer.proof;
                if let Some((_, rank)) = transfer.predecessor {
                    self.delivery = Some(Delivery {
                        endpoint: transfer.endpoint,
                        rank,
                        proof,
                        outstanding: transfer.peers.iter().map(|&peer| (peer, None)).collect(),
                        reply: transfer.command_reply.take(),
                    });
                }
                self.transfer = None;
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(proof));
                }
            }
            _ => {
                // No acknowledgement and no release of the retiring claim on
                // an ambiguous write. Reopening the node rereads durable state.
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Error::NoAuthority));
                }
                if let Some(reply) = transfer.command_reply.take() {
                    let _ = reply.send(Err(ControllerError::ServiceUnavailable));
                }
            }
        }
    }
}

impl ControllerActor {
    pub(super) fn process_publisher_handoff(&mut self, request: Request, wall_now: SystemTime) {
        let (resource, successor, sequence, reply) = match request {
            Request::Prepare {
                principal,
                room,
                reply,
            } => {
                let _ =
                    reply.send(self.prepare_publisher_handoff_target(principal, room, wall_now));
                return;
            }
            Request::Inspect {
                resource,
                room,
                successor,
                sequence,
                reply,
            } => {
                let _ = reply.send(
                    self.inspect_publisher_handoff(resource, room, successor, sequence, wall_now),
                );
                return;
            }
            Request::Start {
                resource,
                successor,
                sequence,
                reply,
            } => (resource, successor, sequence, reply),
        };
        let requester = resource.connection_id;
        let result = self
            .authorize_native_resource(resource, wall_now)
            .and_then(|authorization| {
                let room = authorization.room_id.ok_or(ControllerError::GrantGone)?;
                if !authorization.grants.allows(
                    pulsebeam_core::auth::grants::Action::Join,
                    room.as_str(),
                    None,
                    None,
                    None,
                ) {
                    return Err(ControllerError::AuthorizationRejected);
                }
                let endpoint = EndpointKey {
                    room: ProjectRoomKey {
                        project: authorization.project_id,
                        room,
                    },
                    participant: authorization.participant_id,
                };
                self.request_publisher_handoff(requester, endpoint, successor, sequence, wall_now)
                    .map_err(|error| match error {
                        Error::Expired => ControllerError::AuthorizationExpired,
                        Error::Scope | Error::Retired => ControllerError::GrantGone,
                        Error::Invalid => ControllerError::AuthorizationRejected,
                        Error::Unfresh | Error::NoAuthority => ControllerError::ServiceUnavailable,
                    })
            });
        match result {
            Ok(_receipt) => {
                if let Some(transfer) = self.publisher_handoffs.transfer.as_mut() {
                    transfer.command_reply = Some(reply);
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn prepare_publisher_handoff_target(
        &mut self,
        principal: VerifiedPrincipal,
        room: RoomId,
        wall_now: SystemTime,
    ) -> Result<Owner, ControllerError> {
        let unix_now = wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ControllerError::ServiceUnavailable)?
            .as_secs();
        if principal.expiry().is_expired_at(unix_now) {
            return Err(ControllerError::AuthorizationExpired);
        }
        let authorization = principal
            .authorize_room(room)
            .map_err(|_| ControllerError::AuthorizationRejected)?;
        let Some(Authority::Provisioned(trust)) = &self.receipt_trust else {
            return Err(ControllerError::ServiceUnavailable);
        };
        let now = Instant::now();
        if !self.provisioned_claim_views_ready(trust, now) {
            return Err(ControllerError::ServiceUnavailable);
        }
        let scope = Scope {
            project: authorization.project_id,
            room,
            participant: authorization.participant_id,
        };
        if self.core.registry.get_endpoint(endpoint(scope)).is_some()
            || self
                .publisher_handoffs
                .local_target(endpoint(scope), trust.issuer())
                .is_some()
            || self.publisher_handoffs.store.is_none()
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        self.publisher_handoffs
            .prepare_target(scope, trust.issuer())
    }

    fn inspect_publisher_handoff(
        &self,
        resource: NativeResourceRequest,
        room: RoomId,
        successor: Owner,
        sequence: NonZeroU64,
        wall_now: SystemTime,
    ) -> Result<Status, ControllerError> {
        let NativeResourceRequest {
            principal,
            connection_id,
        } = resource;
        let unix_now = wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ControllerError::ServiceUnavailable)?
            .as_secs();
        if principal.expiry().is_expired_at(unix_now) {
            return Err(ControllerError::AuthorizationExpired);
        }
        // The predecessor's live connection no longer exists after a cutoff.
        // Only the same participant with current join rights may read its result.
        let authorization = principal
            .authorize_room(room)
            .map_err(|_| ControllerError::AuthorizationRejected)?;
        let Some(Authority::Provisioned(trust)) = &self.receipt_trust else {
            return Err(ControllerError::ServiceUnavailable);
        };
        let issuer = trust.issuer();
        let scope = Scope {
            project: authorization.project_id,
            room,
            participant: authorization.participant_id,
        };
        let matches = |handoff: Handoff| {
            handoff.scope == scope
                && handoff.predecessor.node == issuer.node
                && handoff.predecessor.domain == issuer.trust_domain
                && handoff.predecessor.connection == connection_id
                && handoff.successor == successor
                && handoff.sequence == sequence
        };
        let state = &self.publisher_handoffs;
        if state
            .transfer
            .as_ref()
            .is_some_and(|transfer| matches(transfer.proof.handoff))
            || state
                .delivery
                .as_ref()
                .is_some_and(|delivery| matches(delivery.proof.handoff))
        {
            return Ok(Status::Pending);
        }
        let handoff = state
            .store
            .as_ref()
            .ok_or(ControllerError::ServiceUnavailable)?
            .current(scope)
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        Ok(if handoff.is_some_and(matches) {
            Status::Completed
        } else {
            Status::NotFound
        })
    }

    /// The adapter authenticates `requester` as the current connection. Only a
    /// fully applied cutoff and durable write can complete the returned receipt.
    fn request_publisher_handoff(
        &mut self,
        requester: ConnectionId,
        endpoint: EndpointKey,
        successor: Owner,
        sequence: NonZeroU64,
        wall_now: SystemTime,
    ) -> Result<Receipt, Error> {
        let Some(Authority::Provisioned(trust)) = self.receipt_trust.as_ref() else {
            return Err(Error::NoAuthority);
        };
        if !self.provisioned_claim_views_ready(trust, Instant::now()) {
            return Err(Error::Unfresh);
        }
        if successor.node == trust.issuer().node || trust.eligible_key(issuer(successor)).is_none()
        {
            return Err(Error::NoAuthority);
        }
        let meta = self
            .core
            .registry
            .get_endpoint(endpoint)
            .filter(|meta| meta.connection_id == requester && meta.materialized)
            .cloned()
            .ok_or(Error::Scope)?;
        let rank = meta.claim.ok_or(Error::NoAuthority)?;
        if self.peer_lineage_retired(endpoint, rank)
            || !meta
                .authorization
                .as_ref()
                .is_some_and(|lease| !lease.is_expired_at(wall_now) && lease.allows_current_room())
        {
            return Err(Error::NoAuthority);
        }
        let incarnation = Incarnation {
            participant: endpoint.participant,
            connection: requester,
            transport: meta.transport.ok_or(Error::NoAuthority)?,
        };
        if !self.ownership.accepted_incarnation(incarnation)
            || self.publisher_handoffs.transfer.is_some()
            || self.publisher_handoffs.delivery.is_some()
        {
            return Err(Error::NoAuthority);
        }
        let predecessor = owner(trust.issuer(), requester);
        let handoff = Handoff {
            scope: scope(endpoint),
            sequence,
            predecessor,
            successor,
        };
        let store = self
            .publisher_handoffs
            .store
            .as_ref()
            .ok_or(Error::NoAuthority)?;
        if !store.needs_commit(handoff).map_err(|_| Error::Invalid)? {
            return Err(Error::Invalid);
        }
        let proof = Proof::issue(trust.as_ref(), handoff)?;
        Frame::Offer(Box::new(proof)).encode()?;
        let (reply, receiver) = oneshot::channel();
        self.publisher_handoffs.transfer = Some(Transfer {
            endpoint,
            predecessor: Some((incarnation, rank)),
            proof,
            peers: trust.pinned_origins().collect(),
            reply: Some(reply),
            command_reply: None,
            write: None,
        });
        self.terminate_incarnation(
            endpoint.participant,
            meta,
            true,
            ConnectionEnd::Replaced(Some(successor.connection)),
        );
        Ok(receiver)
    }

    pub(super) fn receive_publisher_handoff(
        &mut self,
        session: Session,
        frame: Frame,
    ) -> Option<Box<[u8]>> {
        match frame {
            Frame::Offer(proof) => {
                // A retry can be acknowledged immediately once the identical
                // record has been committed. First receipt starts the write.
                let mut result = self.accept_publisher_handoff(session, *proof).ok()?;
                let committed = result.try_recv().ok()?.ok()?;
                Frame::Committed(committed.signature).encode().ok()
            }
            Frame::Committed(signature) => {
                let remote = self.peer_ownership.session_origin(session).ok()?;
                self.publisher_handoffs.acknowledge(remote, signature);
                None
            }
        }
    }

    pub(super) fn accept_publisher_handoff(
        &mut self,
        session: Session,
        proof: Proof,
    ) -> Result<Receipt, Error> {
        let Some(Authority::Provisioned(trust)) = self.receipt_trust.as_ref() else {
            return Err(Error::NoAuthority);
        };
        if !self.provisioned_claim_views_ready(trust, Instant::now()) {
            return Err(Error::Unfresh);
        }
        let origin = self
            .peer_ownership
            .session_origin(session)
            .map_err(|_| Error::NoAuthority)?;
        proof.verify(trust.as_ref(), origin)?;
        let endpoint = endpoint(proof.handoff.scope);
        if self.publisher_handoffs.transfer.is_some() {
            return Err(Error::NoAuthority);
        }
        let store = self
            .publisher_handoffs
            .store
            .as_ref()
            .ok_or(Error::NoAuthority)?;
        let (reply, receiver) = oneshot::channel();
        if !store
            .needs_commit(proof.handoff)
            .map_err(|_| Error::Invalid)?
        {
            let _ = reply.send(Ok(proof));
            return Ok(receiver);
        }
        if self.core.registry.get_endpoint(endpoint).is_some() {
            return Err(Error::Scope);
        }
        if issuer(proof.handoff.successor) == trust.issuer()
            && self.publisher_handoffs.targets.get(&proof.handoff.scope)
                != Some(&proof.handoff.successor)
        {
            return Err(Error::Scope);
        }
        self.publisher_handoffs.transfer = Some(Transfer {
            endpoint,
            predecessor: None,
            proof,
            peers: Box::new([]),
            reply: Some(reply),
            command_reply: None,
            write: None,
        });
        self.publisher_handoffs.start_write();
        Ok(receiver)
    }
}
