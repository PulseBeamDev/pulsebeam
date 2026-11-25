use super::{
    ControllerActor, ParticipantId, ProjectRoomKey, limits,
    responders::{ReadScope, ReadStep, RoomRead},
    wire,
};
use crate::control::{permissions::ResourcePermissions, registry::ParticipantMeta};
use pulsebeam_core::auth::grants::{Action, RoomDiscover};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control::controller) struct Revision {
    pub(in crate::control::controller) presence: u64,
    pub(in crate::control::controller) publications: u64,
}

/// An authorized read of this node's materialized room, independent of media
/// routes. Holding the controller borrow fixes which state a scan step reads.
pub(in crate::control::controller) struct RoomQuery<'a> {
    scope: ReadScope,
    local: LocalRead<'a>,
}

struct LocalRead<'a> {
    controller: &'a ControllerActor,
    room: ProjectRoomKey,
    permissions: &'a ResourcePermissions,
}

impl<'a> RoomQuery<'a> {
    pub(in crate::control::controller) fn authorized(
        controller: &'a ControllerActor,
        scope: ReadScope,
        permissions: &'a ResourcePermissions,
    ) -> Result<Self, wire::FailureCode> {
        Ok(Self {
            scope,
            local: LocalRead::authorized(controller, scope.room, permissions)?,
        })
    }
}

impl<'a> LocalRead<'a> {
    fn authorized(
        controller: &'a ControllerActor,
        room: ProjectRoomKey,
        permissions: &'a ResourcePermissions,
    ) -> Result<Self, wire::FailureCode> {
        let allowed = match permissions {
            ResourcePermissions::Grants { grants, .. } => grants.may_discover(room.room.as_str()),
            #[cfg(test)]
            ResourcePermissions::RoomBound => true,
            _ => false,
        };
        if !allowed {
            return Err(wire::FailureCode::PermissionDenied);
        }
        Ok(Self {
            controller,
            room,
            permissions,
        })
    }

    fn revision(&self) -> Revision {
        Revision {
            presence: self
                .controller
                .core
                .registry
                .room_catalog_version(self.room),
            publications: self.controller.lifecycle.room_catalog_version(self.room),
        }
    }

    fn next(&self, after: Option<&str>) -> Result<Option<Candidate>, wire::FailureCode> {
        let Some((canonical, participant, publisher)) = self
            .controller
            .core
            .registry
            .ordered_participants_in_room(&self.room, after)
            .next()
        else {
            return Ok(None);
        };
        let entry = catalog_entry(
            self.controller,
            self.permissions,
            self.room,
            self.room.room.as_str(),
            participant,
            publisher,
        )?;
        Ok(Some(Candidate {
            canonical: canonical.to_owned(),
            entry,
        }))
    }
}

impl RoomRead for RoomQuery<'_> {
    fn scope(&self) -> ReadScope {
        self.scope
    }
    fn revision(&self) -> Revision {
        self.local.revision()
    }
    fn next(&self, after: Option<&str>) -> Result<ReadStep, wire::FailureCode> {
        self.local
            .next(after)
            .map(|row| row.map_or(ReadStep::Exhausted, ReadStep::Row))
    }
}

#[derive(Clone)]
pub(in crate::control::controller) struct Candidate {
    pub(in crate::control::controller) canonical: String,
    pub(in crate::control::controller) entry: Option<wire::CatalogParticipantEntry>,
}

const NEIGHBOR_SCAN: usize = 16;

pub(in crate::control::controller) struct NeighborRead<'a> {
    pub(in crate::control::controller) project: super::ProjectId,
    pub(in crate::control::controller) discover: RoomDiscover,
    pub(in crate::control::controller) after: Option<&'a str>,
    pub(in crate::control::controller) revision: Option<Revision>,
}

pub(in crate::control::controller) enum NeighborWindow {
    Row {
        revision: Revision,
        candidate: Candidate,
    },
    ScanMore {
        revision: Revision,
        scanned_through: String,
    },
    Exhausted {
        revision: Revision,
    },
}

/// Serve one bounded window under the neighbor's attenuated, association-bound
/// Discover assertion. The assertion must be authenticated before this call.
pub(in crate::control::controller) fn read_neighbor(
    controller: &ControllerActor,
    request: NeighborRead<'_>,
) -> Result<NeighborWindow, wire::FailureCode> {
    let room = ProjectRoomKey {
        project: request.project,
        room: super::RoomId::new(request.discover.room())
            .map_err(|_| wire::FailureCode::InvalidConfiguration)?,
    };
    let grants = request
        .discover
        .into_grants()
        .map_err(|_| wire::FailureCode::PermissionDenied)?;
    let permissions = ResourcePermissions::Grants {
        joined_room: None,
        grants,
    };
    let read = LocalRead::authorized(controller, room, &permissions)?;
    read_window(&read, request.after, request.revision)
}

fn read_window(
    read: &LocalRead<'_>,
    after: Option<&str>,
    expected: Option<Revision>,
) -> Result<NeighborWindow, wire::FailureCode> {
    let revision = read.revision();
    if expected.is_some_and(|expected| expected != revision) {
        return Err(wire::FailureCode::ResourceGone);
    }
    let mut last = None;
    for _ in 0..NEIGHBOR_SCAN {
        let Some(candidate) = read.next(last.as_deref().or(after))? else {
            return Ok(NeighborWindow::Exhausted { revision });
        };
        if candidate.entry.is_some() {
            return Ok(NeighborWindow::Row {
                revision,
                candidate,
            });
        }
        last = Some(candidate.canonical);
    }
    Ok(NeighborWindow::ScanMore {
        revision,
        scanned_through: last.ok_or(wire::FailureCode::InternalFailure)?,
    })
}

fn catalog_entry(
    controller: &ControllerActor,
    permissions: &ResourcePermissions,
    room: ProjectRoomKey,
    literal: &str,
    participant: ParticipantId,
    publisher: &ParticipantMeta,
) -> Result<Option<wire::CatalogParticipantEntry>, wire::FailureCode> {
    if !publisher.materialized {
        return Ok(None);
    }

    let presence = permissions.allows_at_room(
        Action::Discover,
        literal,
        Some(participant.as_str()),
        None,
        None,
    );
    let mut tracks = Vec::new();
    for (index, track) in controller
        .lifecycle
        .publisher_sources(room, participant, publisher.connection_id)
        .enumerate()
    {
        if index >= limits::MAX_TRACKS_PER_PARTICIPANT {
            return Err(wire::FailureCode::CapacityExceeded);
        }
        if !publisher.authorization.as_ref().is_some_and(|lease| {
            !lease.is_expired_at(SystemTime::now())
                && lease
                    .permissions()
                    .allows_source(Action::Publish, None, track)
        }) {
            continue;
        }
        if permissions.allows_source_at_room(
            Action::Discover,
            literal,
            Some(participant.as_str()),
            track,
        ) {
            tracks.push(
                crate::control::native::track_descriptor(track)
                    .ok_or(wire::FailureCode::InternalFailure)?,
            );
        }
    }
    if !presence && tracks.is_empty() {
        return Ok(None);
    }
    tracks.sort_by(|left, right| left.publication_id.cmp(&right.publication_id));
    Ok(Some(wire::CatalogParticipantEntry {
        participant: Some(wire::Participant {
            participant_id: participant.as_str().to_owned(),
            room_id: literal.to_owned(),
        }),
        tracks,
    }))
}
