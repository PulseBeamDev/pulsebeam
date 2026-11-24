use std::num::NonZeroU64;
use std::time::{SystemTime, UNIX_EPOCH};

use pulsebeam_agent_quic::records::{Admit, Denial};
use pulsebeam_core::auth::authenticate_participant_token;
use pulsebeam_core::identity::{ConnectionId, RoomId};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    AdmissionProjection, AttemptKey, AuthorizationLease, ControllerActor, ControllerError,
    MaterializationReply, OwnedAdmissionError, OwnedPreparation, ParticipantState,
    PendingMaterialization, RequiredAction, SignalingProtocol, ownership, quic_placement,
};
use crate::{
    control::{
        api::creation,
        native::{handler::StateProtocolHandler, handoff},
        sdp::Selection,
    },
    entity::ConnectionScope,
    id::ShardId,
    shard::worker::ShardCommand,
};

pub(crate) type Reply = oneshot::Sender<Result<creation::Delivery, Denial>>;

pub(crate) struct Command {
    pub shard: ShardId,
    pub association: NonZeroU64,
    pub body: Vec<u8>,
    pub reply: Reply,
}

pub(super) fn denial(error: &OwnedAdmissionError) -> Denial {
    match error {
        OwnedAdmissionError::Request(
            ControllerError::AuthorizationRejected | ControllerError::AuthorizationExpired,
        ) => Denial::Denied,
        OwnedAdmissionError::Authority(ownership::Error::Capacity)
        | OwnedAdmissionError::Request(ControllerError::ServiceUnavailable)
        | OwnedAdmissionError::Image(_) => Denial::Capacity,
        _ => Denial::AttemptUnavailable,
    }
}

/// An authenticated admission candidate, not authorization to attach to any
/// transport. Only the controller's ownership transaction may commit it.
/// Never derive Debug: the successor proof is a credential.
pub(super) struct Redeem<'a> {
    pub(super) target: quic_placement::Target,
    pub(super) connection: ConnectionId,
    pub(super) partition: Option<usize>,
    pub(super) lease: AuthorizationLease,
    pub(super) proof: ownership::Proof,
    pub(super) sequence: NonZeroU64,
    pub(super) binding: ownership::RequestBinding,
    pub(super) successor_receipt: Option<&'a [u8]>,
}

impl ControllerActor {
    pub(super) fn submit_quic_redeem(&mut self, command: Command, wall_now: SystemTime) {
        let Command {
            shard,
            association,
            body,
            reply,
        } = command;
        if reply.is_closed() {
            return;
        }
        let Some(permit) = self.delivery_budget.reserve() else {
            let _ = reply.send(Err(Denial::Capacity));
            return;
        };
        let result = self.begin_quic_redeem(shard, association, &body, wall_now);
        match result {
            Ok(OwnedPreparation::Start(_, pending)) => {
                self.start_materialization(RequiredAction::Materialize {
                    pending: *pending,
                    reply: MaterializationReply::Quic(reply, permit),
                });
            }
            // A new QUIC association cannot attach to pending or accepted work.
            Ok(_) => {
                let _ = reply.send(Err(Denial::AttemptUnavailable));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn begin_quic_redeem(
        &mut self,
        shard: ShardId,
        association: NonZeroU64,
        body: &[u8],
        wall_now: SystemTime,
    ) -> Result<OwnedPreparation, Denial> {
        let now = Instant::now();
        let Redeem {
            target,
            connection,
            partition,
            lease,
            proof,
            sequence,
            binding,
            successor_receipt,
        } = self.preflight_quic_redeem(shard, body, wall_now, now)?;
        let state = ParticipantState {
            selection: Selection::Native,
            room_id: target.scope.room,
            participant_id: target.participant,
            connection_id: connection,
            old_connection_id: None,
            authorization: Some(lease),
            profile: SignalingProtocol::State,
            rtp_capabilities: Some(crate::control::rtp::quic_capabilities()),
        };
        self.prepare_owned_attempt(
            state,
            AttemptKey {
                proof,
                sequence,
                binding,
                successor_receipt: successor_receipt.map(<[u8]>::to_vec),
            },
            wall_now,
            |actor, mut state| {
                let authorization = state
                    .authorization
                    .take()
                    .ok_or(ControllerError::AuthorizationRejected)?;
                let (control, handoff) = handoff::pair(connection, actor.native_ready_tx.clone());
                let mut native = StateProtocolHandler::with_binding_profile(
                    target.participant,
                    target.scope.room,
                    state.rtp_capabilities.take(),
                    control,
                    pulsebeam_proto::state::validation::BindingProfile::Quic,
                )
                .map_err(|_| ControllerError::ServiceUnavailable)?;
                let expiry = authorization
                    .native_expiry()
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                native
                    .model
                    .authorization(
                        expiry,
                        authorization.revision(),
                        authorization.permissions().clone(),
                    )
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                let _ = actor
                    .quic_placements
                    .take(proof)
                    .ok_or(ControllerError::ServiceUnavailable)?;
                let transport = actor.core.reserve_transport(shard, now);
                let (ack_tx, ack_rx) = oneshot::channel();
                Ok(PendingMaterialization {
                    native: Some(native),
                    shard,
                    partition_index: partition,
                    command: Some(ShardCommand::MaterializeQuic {
                        association,
                        connection,
                        participant: target.participant,
                        transport,
                        handoff: Box::new(handoff),
                        grant: crate::shard::participants::QuicGrant {
                            revision: authorization.revision(),
                            expiry: authorization.expiry(),
                        },
                        scope: target.scope,
                        ack: ack_tx,
                    }),
                    ack: Some(ack_rx),
                    participant: target.participant,
                    claim: ownership::ClaimRank::root(connection),
                    authorization: Some(authorization),
                    profile: state.profile,
                    transport,
                    scope: target.scope,
                    answer: String::new(),
                    whep_bindings: Default::default(),
                    owned: None,
                })
            },
            &AdmissionProjection::Quic,
        )
        .map_err(|error| denial(&error))
    }

    pub(super) fn preflight_quic_redeem<'a>(
        &self,
        shard: ShardId,
        body: &'a [u8],
        wall_now: SystemTime,
        now: Instant,
    ) -> Result<Redeem<'a>, Denial> {
        let admit = Admit::decode(body).map_err(|_| Denial::Denied)?;
        let time = wall_now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Denial::Denied)?;
        let registry = self.key_registry.as_ref().ok_or(Denial::Denied)?;
        let principal = authenticate_participant_token(registry, admit.token, time.as_secs())
            .map_err(|_| Denial::Denied)?;
        let room = admit
            .room
            .map(RoomId::new)
            .transpose()
            .map_err(|_| Denial::Denied)?;
        let authorization = principal
            .authorize_presence(room)
            .map_err(|_| Denial::Denied)?;
        let proof = ownership::Proof::parse(admit.owner).map_err(|_| Denial::Denied)?;
        let target = quic_placement::Target {
            scope: ConnectionScope {
                project: authorization.project_id,
                room,
            },
            participant: authorization.participant_id,
            shard,
        };
        let placement = self
            .quic_placements
            .get(proof, target, &self.core.registry, now)
            .ok_or(Denial::Placement)?;
        if placement.successor.as_deref() != admit.successor {
            return Err(Denial::Placement);
        }
        let lease = AuthorizationLease::from_verified(authorization, wall_now, now)
            .map_err(|_| Denial::Denied)?;
        lease.native_expiry().map_err(|_| Denial::Denied)?;
        let binding = ownership::RequestBinding::for_quic(body).map_err(|_| Denial::Denied)?;
        Ok(Redeem {
            target,
            connection: placement.connection,
            partition: placement.partition,
            lease,
            proof,
            sequence: admit.attempt,
            binding,
            successor_receipt: admit.successor,
        })
    }
}
