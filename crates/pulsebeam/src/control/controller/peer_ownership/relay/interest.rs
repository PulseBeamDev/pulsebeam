//! Short, authenticated viewer-edge assertions on the direct ownership lane.

use std::time::{Duration, SystemTime};

use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use crate::{
    control::controller::ControllerActor,
    entity::{
        ConnectionId, ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind,
    },
    id::ShardId,
    shard_update::PublisherInterest,
};

const TAG: &[u8] = &[1, 0x17, 0];
pub(in crate::control::controller) const TTL: Duration = Duration::from_secs(3);
pub(in crate::control::controller) const BATCH: usize = 3;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control::controller) struct Assertion {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    connection: ConnectionId,
    video: bool,
    expires_ms: u64,
}

pub(in crate::control::controller) fn millis(wall: SystemTime) -> Option<u64> {
    wall.duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

impl Assertion {
    pub(in crate::control::controller) fn for_track(
        actor: &ControllerActor,
        track: &crate::track::Track,
        wall: SystemTime,
        limit_ms: u64,
    ) -> Option<Self> {
        let video = match track.id().kind() {
            TrackKind::Video => true,
            TrackKind::Audio => false,
            TrackKind::Data => return None,
        };
        let source = track.meta();
        Some(Self {
            project: source.room.project,
            room: source.room.room,
            publication: source.id.publication_id(),
            connection: source.connection_id,
            video,
            expires_ms: actor.direct_media_interest_expiry(track, wall, limit_ms)?,
        })
    }

    pub(in crate::control::controller) fn resolve(
        self,
        actor: &ControllerActor,
        now: Instant,
        wall_ms: u64,
    ) -> Option<(ShardId, PublisherInterest)> {
        let ttl = Duration::from_millis(self.expires_ms.checked_sub(wall_ms)?);
        if ttl.is_zero() || ttl > TTL {
            return None;
        }
        let (shard, publisher) = actor.lifecycle.network_publisher_anchor(
            ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            PublicationKey::new(
                if self.video {
                    TrackKind::Video
                } else {
                    TrackKind::Audio
                },
                self.publication,
            ),
            &actor.core.registry,
        )?;
        (publisher.connection == self.connection).then_some((
            shard,
            PublisherInterest {
                publisher,
                expires: now.checked_add(ttl)?,
            },
        ))
    }
}

pub(in crate::control::controller) fn encode(assertions: &[Assertion]) -> Option<Box<[u8]>> {
    if assertions.is_empty() || assertions.len() > BATCH {
        return None;
    }
    let mut bytes = TAG.to_vec();
    serde_json::to_writer(&mut bytes, assertions).ok()?;
    (bytes.len() <= pulsebeam_link::CONTROL_FRAME_BYTES).then(|| bytes.into_boxed_slice())
}

pub(in crate::control::controller) fn decode(bytes: &[u8]) -> Option<Vec<Assertion>> {
    if bytes.len() > pulsebeam_link::CONTROL_FRAME_BYTES {
        return None;
    }
    let assertions: Vec<Assertion> = serde_json::from_slice(bytes.strip_prefix(TAG)?).ok()?;
    (!assertions.is_empty() && assertions.len() <= BATCH).then_some(assertions)
}
