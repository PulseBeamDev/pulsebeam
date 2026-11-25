//! An origin's short-lived assertion about a media source. A permit is not a
//! claim, subscriber grant, or send authorization. Neither a gateway's stream
//! pin nor a WAN receive ACK is an origin verification key.

use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;
use pulsebeam_core::auth::ApiSigningKey;
use pulsebeam_proto::state::Track;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

use crate::{
    cluster::{RegionTree, mesh::Member},
    control::{
        controller::{
            ControllerActor,
            ownership::receipt::Authority,
            peer_ownership::relay::wan_source::{WanSourceWindow, WireWanSourceWindow},
        },
        native::track_descriptor,
    },
    entity::{EndpointKey, ParticipantId, ProjectId, ProjectRoomKey, RoomId, TrackId, TrackKind},
};

const DOMAIN: &[u8] = b"pulsebeam/regional-media-source/v1\0";
const SIGNATURE_BYTES: usize = 64;
const MAX_BYTES: usize = 4096;
const MAX_LEASE_MS: u64 = 3_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Statement {
    tree: [u8; 32],
    issued_ms: u64,
    expires_ms: u64,
    participant: ParticipantId,
    track: TrackId,
    descriptor: [u8; 32],
    source: WireWanSourceWindow,
}

#[derive(Clone, Copy)]
pub(super) struct VerifiedSource {
    pub(super) source: WanSourceWindow,
    pub(super) issued_ms: u64,
    pub(super) expires_ms: u64,
}

pub(super) fn unix_millis(now: SystemTime) -> Option<u64> {
    now.duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

pub(super) fn source_name(
    project: ProjectId,
    descriptor: &Track,
) -> Option<(EndpointKey, TrackId)> {
    Some((
        EndpointKey {
            room: ProjectRoomKey {
                project,
                room: RoomId::new(&descriptor.room_id).ok()?,
            },
            participant: ParticipantId::new(&descriptor.participant_id).ok()?,
        },
        TrackId::new(&descriptor.track_id).ok()?,
    ))
}

fn descriptor_hash(descriptor: &Track) -> [u8; 32] {
    Sha3_256::digest(descriptor.encode_to_vec()).into()
}

fn matches_descriptor(
    source: WanSourceWindow,
    endpoint: EndpointKey,
    track: TrackId,
    descriptor: &Track,
) -> bool {
    use pulsebeam_proto::state::track::Kind;
    source.room == endpoint.room
        && descriptor.room_id == source.room.room.as_str()
        && descriptor.participant_id == endpoint.participant.as_str()
        && descriptor.track_id == track.as_str()
        && descriptor.publication_id == source.publisher.publication.as_str()
        && matches!(
            (
                source.publisher.publication.kind(),
                descriptor.kind.as_ref()
            ),
            (TrackKind::Audio, Some(Kind::Audio(_)))
                | (TrackKind::Video, Some(Kind::Video(_)))
                | (TrackKind::Data, Some(Kind::Data(_)))
        )
}

pub(super) fn current_publisher(
    actor: &ControllerActor,
    source: WanSourceWindow,
    now: SystemTime,
) -> Option<(&crate::track::Track, u64)> {
    actor
        .receipt_trust
        .as_ref()
        .filter(|authority| Authority::ready(authority))?;
    let track = actor
        .lifecycle
        .source_exact(source.room, source.publisher.publication)?;
    let endpoint = EndpointKey {
        room: source.room,
        participant: track.meta().origin,
    };
    let current = actor.core.registry.get_endpoint(endpoint)?;
    let claim = current.claim?;
    let lease = current.authorization.as_ref()?;
    if current.connection_id != source.publisher.connection
        || lease.is_expired_at(now)
        || !lease.allows_current_room()
        || actor.peer_lineage_retired(endpoint, claim)
        || actor
            .lifecycle
            .network_publisher_anchor(
                source.room,
                source.publisher.publication,
                &actor.core.registry,
            )?
            .1
            != source.publisher
    {
        return None;
    }
    Some((track, lease.expiry().unix_seconds().saturating_mul(1_000)))
}

pub(super) fn issue(
    actor: &ControllerActor,
    tree: &RegionTree,
    origin: Member,
    key: &ApiSigningKey,
    mut source: WanSourceWindow,
    now: SystemTime,
) -> Option<(Track, Box<[u8]>)> {
    let (track, expires_ms) = current_publisher(actor, source, now)?;
    source.origin = Some(origin);
    source.offer_cut = None;
    let descriptor = track_descriptor(track)?;
    let assertion = SourceAssertion {
        endpoint: EndpointKey {
            room: source.room,
            participant: track.meta().origin,
        },
        track: track.meta().track_id?,
        descriptor: &descriptor,
        source,
    };
    let permit = sign(tree, key, assertion, now, expires_ms)?;
    Some((descriptor, permit))
}

#[derive(Clone, Copy)]
struct SourceAssertion<'a> {
    endpoint: EndpointKey,
    track: TrackId,
    descriptor: &'a Track,
    source: WanSourceWindow,
}

fn sign(
    tree: &RegionTree,
    key: &ApiSigningKey,
    SourceAssertion {
        endpoint,
        track,
        descriptor,
        source,
    }: SourceAssertion<'_>,
    now: SystemTime,
    expiry_limit_ms: u64,
) -> Option<Box<[u8]>> {
    if source.offer_cut.is_some()
        || tree.media_key(source.origin?)? != key.public_key()
        || !matches_descriptor(source, endpoint, track, descriptor)
    {
        return None;
    }
    let issued_ms = unix_millis(now)?;
    let expires_ms = issued_ms.checked_add(MAX_LEASE_MS)?.min(expiry_limit_ms);
    if expires_ms <= issued_ms {
        return None;
    }
    let mut signed = DOMAIN.to_vec();
    serde_json::to_writer(
        &mut signed,
        &Statement {
            tree: *tree.fingerprint(),
            issued_ms,
            expires_ms,
            participant: endpoint.participant,
            track,
            descriptor: descriptor_hash(descriptor),
            source: source.into(),
        },
    )
    .ok()?;
    if signed.len().saturating_add(SIGNATURE_BYTES) > MAX_BYTES {
        return None;
    }
    signed.extend_from_slice(&key.sign_message(&signed));
    Some(signed.into_boxed_slice())
}

pub(super) fn verify(
    tree: &RegionTree,
    origin: Member,
    expected: EndpointKey,
    track: TrackId,
    descriptor: &Track,
    permit: &[u8],
    now: SystemTime,
) -> Option<VerifiedSource> {
    if permit.len() > MAX_BYTES {
        return None;
    }
    let (signed, signature) =
        permit.split_at_checked(permit.len().checked_sub(SIGNATURE_BYTES)?)?;
    let claim: Statement = serde_json::from_slice(signed.strip_prefix(DOMAIN)?).ok()?;
    let now = unix_millis(now)?;
    if claim.issued_ms > now
        || claim.expires_ms <= now
        || claim.expires_ms.checked_sub(claim.issued_ms)? > MAX_LEASE_MS
        || &claim.tree != tree.fingerprint()
        || claim.participant != expected.participant
        || claim.track != track
        || claim.descriptor != descriptor_hash(descriptor)
    {
        return None;
    }
    let source = claim.source.decode()?;
    if source.offer_cut.is_some()
        || source.origin != Some(origin)
        || !matches_descriptor(source, expected, track, descriptor)
        || !tree
            .media_key(origin)?
            .verify_message(signed, signature.try_into().ok()?)
    {
        return None;
    }
    Some(VerifiedSource {
        source,
        issued_ms: claim.issued_ms,
        expires_ms: claim.expires_ms,
    })
}

#[cfg(test)]
mod tests;
