use std::{
    num::NonZeroU64,
    time::{SystemTime, UNIX_EPOCH},
};

use pulsebeam_core::auth::grants::Action;
use pulsebeam_link::{LinkTime, PeerControl};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    ControllerActor, ControllerError, NativeResourceRequest, StandbyControlCommand,
    ownership::{
        ClaimRank,
        receipt::{
            Authority, PeerTrust,
            standby::{Candidates, Grant, Owner, Source, wire::Frame},
        },
    },
};
use crate::entity::{ConnectionId, EndpointKey, PublicationKey, TrackId, TrackKind};

#[cfg(feature = "sim")]
pub(super) mod sim;

const CAPACITY: usize = 128;
const SEND_BUDGET: usize = 4;

pub(super) enum Operation {
    Grant {
        source: Source,
        expires_at: u64,
    },
    Select {
        revision: NonZeroU64,
        delegated: Option<(Source, Grant)>,
    },
    Advertise {
        owner: Owner,
        grant: Grant,
    },
}

pub(super) struct Request {
    pub(super) resource: NativeResourceRequest,
    pub(super) publication: PublicationKey,
    pub(super) operation: Operation,
    pub(super) reply: oneshot::Sender<Result<Receipt, ControllerError>>,
}

pub(super) enum Receipt {
    Granted { owner: Box<Owner>, grant: Grant },
    Queued,
}

enum Prepared {
    Granted { owner: Owner, grant: Grant },
    Queue(Entry),
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StandbyControlRequest([redacted])")
    }
}

impl StandbyControlCommand {
    pub(super) fn new(request: Request) -> Self {
        Self(Box::new(request))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Selection(EndpointKey, TrackKind, TrackId),
    Advertisement(EndpointKey, PublicationKey),
}

struct Entry {
    key: Key,
    requester: EndpointKey,
    connection: ConnectionId,
    rank: ClaimRank,
    authorization: u64,
    publication: PublicationKey,
    expires_at: u64,
    revision: Option<NonZeroU64>,
    bytes: Box<[u8]>,
}

#[derive(Default)]
pub(super) struct Outbox(Vec<Entry>);

impl Outbox {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn queue(&mut self, entry: Entry) -> Result<(), ControllerError> {
        if let Some(previous) = self.0.iter_mut().find(|previous| previous.key == entry.key) {
            if previous.connection == entry.connection
                && previous.authorization == entry.authorization
                && (entry.revision < previous.revision
                    || (entry.revision.is_some()
                        && entry.revision == previous.revision
                        && entry.bytes != previous.bytes))
            {
                return Err(ControllerError::AuthorizationRejected);
            }
            *previous = entry;
        } else {
            if self.0.len() >= CAPACITY {
                return Err(ControllerError::ServiceUnavailable);
            }
            self.0.push(entry);
        }
        Ok(())
    }

    pub(super) fn send(&self, lane: &PeerControl, expires_at: LinkTime, cursor: &mut usize) {
        for _ in 0..self.0.len().min(SEND_BUDGET) {
            let Some(index) = cursor.checked_rem(self.0.len()) else {
                return;
            };
            let Some(entry) = self.0.get(index) else {
                break;
            };
            if lane.try_send(entry.bytes.clone(), expires_at).is_err() {
                break;
            }
            *cursor = index.saturating_add(1);
        }
    }
}

impl ControllerActor {
    pub(super) fn process_standby_control(
        &mut self,
        request: Request,
        now: Instant,
        wall_now: SystemTime,
    ) {
        let Request {
            resource,
            publication,
            operation,
            reply,
        } = request;
        let result = self
            .prepare_standby_control(resource, publication, operation, now, wall_now)
            .and_then(|prepared| match prepared {
                Prepared::Granted { owner, grant } => Ok(Receipt::Granted {
                    owner: Box::new(owner),
                    grant,
                }),
                Prepared::Queue(entry) => {
                    self.standby_outbox.queue(entry).map(|()| Receipt::Queued)
                }
            });
        let _ = reply.send(result);
    }

    fn prepare_standby_control(
        &self,
        resource: NativeResourceRequest,
        publication: PublicationKey,
        operation: Operation,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<Prepared, ControllerError> {
        let Some(Authority::Provisioned(trust)) = self.receipt_trust.as_ref() else {
            return Err(ControllerError::ServiceUnavailable);
        };
        if !self.provisioned_claim_views_ready(trust, now) {
            return Err(ControllerError::ServiceUnavailable);
        }
        if !matches!(publication.kind(), TrackKind::Audio | TrackKind::Video) {
            return Err(ControllerError::AuthorizationRejected);
        }
        let connection = resource.connection_id;
        let authorization = self.authorize_native_resource(resource, wall_now)?;
        let room = authorization.room_id.ok_or(ControllerError::GrantGone)?;
        let requester = EndpointKey {
            room: crate::entity::ProjectRoomKey {
                project: authorization.project_id,
                room,
            },
            participant: authorization.participant_id,
        };
        let track = self
            .lifecycle
            .source_exact(requester.room, publication)
            .filter(|track| {
                track.meta().connection_id == connection
                    && track.meta().origin == requester.participant
            })
            .ok_or(ControllerError::SourceUnavailable)?;
        let label = track
            .meta()
            .track_id
            .ok_or(ControllerError::SourceUnavailable)?;
        let kind = super::super::permissions::grant_kind(track.kind());
        if !authorization
            .grants
            .allows(Action::Join, room.as_str(), None, None, None)
            || !authorization.grants.allows(
                Action::Publish,
                room.as_str(),
                None,
                Some(kind),
                Some(label.as_str()),
            )
        {
            return Err(ControllerError::AuthorizationRejected);
        }
        let issuer = trust.issuer();
        let meta = self
            .core
            .registry
            .get_endpoint(requester)
            .filter(|meta| meta.materialized && meta.connection_id == connection)
            .ok_or(ControllerError::GrantGone)?;
        let rank = meta.claim.ok_or(ControllerError::GrantGone)?;
        if self.peer_lineage_retired(requester, rank)
            || !self.handoff_permits(requester, issuer, connection)
        {
            return Err(ControllerError::GrantGone);
        }
        let lease = meta
            .authorization
            .as_ref()
            .filter(|lease| {
                !lease.is_expired_at(wall_now)
                    && lease.allows_current_room()
                    && lease.permissions().allows(
                        Action::Publish,
                        None,
                        Some(kind),
                        Some(label.as_str()),
                    )
            })
            .ok_or(ControllerError::AuthorizationRejected)?;
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::AuthorizationExpired)?
            .as_secs();
        let local = Source {
            node: issuer.node,
            generation: issuer.incarnation,
            connection,
            publication,
            track: label,
        };
        let local_owner = Owner {
            endpoint: requester,
            issuer,
            rank,
            authorization: lease.revision(),
            expires_at: lease.expiry().unix_seconds(),
        };
        let (frame, expires_at, key, revision) = match operation {
            Operation::Grant { source, expires_at } => {
                if expires_at > authorization.expiry.unix_seconds() {
                    return Err(ControllerError::AuthorizationExpired);
                }
                let mut candidates = Candidates::new(local_owner, local)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                let grant = local_owner
                    .issue(trust.as_ref(), source, expires_at, unix_now)
                    .map_err(|error| match error {
                        super::ownership::receipt::Error::Expired => {
                            ControllerError::AuthorizationExpired
                        }
                        _ => ControllerError::AuthorizationRejected,
                    })?;
                candidates
                    .admit(local_owner, trust.as_ref(), source, grant, unix_now)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                return Ok(Prepared::Granted {
                    owner: local_owner,
                    grant,
                });
            }
            Operation::Select {
                revision,
                delegated,
            } => {
                let owner = local_owner;
                let mut candidates = Candidates::new(owner, local)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                let source = if let Some((source, grant)) = delegated {
                    candidates
                        .admit(owner, trust.as_ref(), source, grant, unix_now)
                        .map_err(|_| ControllerError::AuthorizationRejected)?;
                    source
                } else {
                    local
                };
                candidates
                    .select(owner, trust.as_ref(), revision, source, unix_now)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                let (_, _, expiry) = candidates
                    .selection(owner, trust.as_ref(), unix_now)
                    .ok_or(ControllerError::AuthorizationRejected)?;
                (
                    Frame::Select {
                        owner,
                        primary: local,
                        revision,
                        delegated,
                    },
                    expiry,
                    Key::Selection(requester, publication.kind(), label),
                    Some(revision),
                )
            }
            Operation::Advertise { owner, grant } => {
                if owner.endpoint.room != requester.room {
                    return Err(ControllerError::AuthorizationRejected);
                }
                let session = self
                    .peer_ownership
                    .active_session(owner.issuer.node, owner.issuer.incarnation)
                    .ok_or(ControllerError::ServiceUnavailable)?;
                let authority = self
                    .standby_authority(session, owner, now)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                let expiry = owner
                    .validate(authority, local, grant, unix_now)
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
                (
                    Frame::Encoder {
                        owner,
                        source: local,
                        grant,
                        publisher: requester.participant,
                    },
                    expiry,
                    Key::Advertisement(owner.endpoint, publication),
                    None,
                )
            }
        };
        Ok(Prepared::Queue(Entry {
            key,
            requester,
            connection,
            rank,
            authorization: lease.revision(),
            publication,
            revision,
            expires_at: expires_at
                .min(authorization.expiry.unix_seconds())
                .min(lease.expiry().unix_seconds()),
            bytes: frame
                .encode()
                .map_err(|_| ControllerError::SourceUnavailable)?,
        }))
    }

    pub(super) fn refresh_standby_outbox(&mut self, now: Instant, wall_now: SystemTime) {
        if self.standby_outbox.is_empty() {
            return;
        }
        let Some(Authority::Provisioned(trust)) = self.receipt_trust.as_ref() else {
            self.standby_outbox.0.clear();
            return;
        };
        let issuer = trust.issuer();
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut outbox = std::mem::take(&mut self.standby_outbox);
        outbox.0.retain(|entry| {
            unix_now < entry.expires_at
                && !self.peer_lineage_retired(entry.requester, entry.rank)
                && self.handoff_permits(entry.requester, issuer, entry.connection)
                && self
                    .core
                    .registry
                    .get_endpoint(entry.requester)
                    .is_some_and(|meta| {
                        meta.connection_id == entry.connection
                            && meta.claim == Some(entry.rank)
                            && meta.materialized
                            && meta.authorization.as_ref().is_some_and(|lease| {
                                lease.revision() == entry.authorization && now < lease.deadline()
                            })
                    })
                && self
                    .lifecycle
                    .source_exact(entry.requester.room, entry.publication)
                    .is_some_and(|track| {
                        track.meta().connection_id == entry.connection
                            && track.meta().origin == entry.requester.participant
                    })
        });
        self.standby_outbox = outbox;
    }
}
