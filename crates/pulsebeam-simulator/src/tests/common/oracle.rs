use super::client::ClientObservation;
use anyhow::{Result, ensure};
use pulsebeam::entity::{ConnectionId, ParticipantId, RoomExternalId, RoomId, TrackId, TrackKind};
use pulsebeam::sim_observation::{ControlObservation, Observation, SelectorObservation};
use pulsebeam::track::SelectionPolicy;
use pulsebeam_agent_native::agent_core::{MediaKind, Publication, TopicMode, TopicRegistrations};
use pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Plan identities are bound at admission. Publication identities are derived by
/// the canonical identity owner, never learned from a receiver or forwarded RTP.
#[derive(Clone, Debug)]
pub struct ExpectedParticipant {
    pub id: ParticipantId,
    pub room: &'static str,
    pub video: bool,
    pub audio: bool,
    pub required: bool,
    pub connection: Option<ConnectionId>,
    pub topics: TopicRegistrations,
    pub video_targets: HashSet<TrackId>,
}

impl ExpectedParticipant {
    pub fn room_id(&self) -> Result<RoomId> {
        Ok(RoomId::derive(
            &DEVELOPMENT_PROJECT_ID,
            &RoomExternalId::new(self.room)?,
        ))
    }

    pub fn track_id(&self, kind: TrackKind, label: &str) -> TrackId {
        self.connection
            .expect("admitted plan participant")
            .derive_track_id(kind, label)
    }

    fn tracks(&self) -> impl Iterator<Item = (TrackId, String)> + '_ {
        self.publications()
            .map(|publication| {
                (
                    publication.id.parse().expect("canonical publication"),
                    publication.label,
                )
            })
            .chain(self.topics.publishers.iter().map(|publisher| {
                let lane = match publisher.mode {
                    TopicMode::Latest => "rt",
                    TopicMode::Ordered => "rel",
                };
                let label = format!("v1/{lane}/{}", publisher.topic);
                (self.track_id(TrackKind::Data, &label), label)
            }))
    }

    fn publications(&self) -> impl Iterator<Item = Publication> + '_ {
        [
            (self.video, TrackKind::Video, MediaKind::Video, "video"),
            (self.audio, TrackKind::Audio, MediaKind::Audio, "audio"),
        ]
        .into_iter()
        .filter(|(active, ..)| *active)
        .map(|(_, kind, media, label)| Publication {
            id: self.track_id(kind, label).as_str(),
            participant_id: self.id.as_str(),
            kind: media,
            label: label.into(),
        })
    }
}

pub struct ControlStateOracle {
    pub participants: Vec<ExpectedParticipant>,
}

impl ControlStateOracle {
    pub fn check_client(
        &self,
        observer: &ExpectedParticipant,
        observation: &ClientObservation,
        requested_video: &[String],
    ) -> Result<()> {
        let snapshot = &observation.snapshot;
        ensure!(
            snapshot.participant_id.as_deref() == Some(&observer.id.as_str()),
            "wrong observer identity"
        );
        ensure!(
            snapshot.room_external_id.as_deref() == Some(observer.room),
            "wrong observer room"
        );
        let peers = self
            .participants
            .iter()
            .filter(|peer| peer.id != observer.id && peer.room == observer.room)
            .collect::<Vec<_>>();
        let allowed = peers
            .iter()
            .map(|peer| peer.id.as_str())
            .collect::<BTreeSet<_>>();
        let required = peers
            .iter()
            .filter(|peer| peer.required)
            .map(|peer| peer.id.as_str())
            .collect::<BTreeSet<_>>();
        let actual = snapshot
            .participants
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        ensure!(
            required.is_subset(&actual) && actual.is_subset(&allowed),
            "participant catalog differs from plan: required={required:?}, allowed={allowed:?}, actual={actual:?}"
        );
        let publications = peers
            .iter()
            .flat_map(|peer| peer.publications())
            .map(|publication| (publication.id.clone(), publication))
            .collect::<BTreeMap<_, _>>();
        for (id, publication) in &snapshot.publications {
            ensure!(
                publications.get(id) == Some(publication),
                "unexpected publication or wrong owner/kind: {publication:?}"
            );
        }
        for peer in peers.iter().filter(|peer| peer.required) {
            for publication in peer.publications() {
                ensure!(
                    snapshot.publications.get(&publication.id) == Some(&publication),
                    "missing publication: {publication:?}"
                );
            }
        }
        ensure!(
            observation.sent_intent_revision == Some(snapshot.accepted_intent_revision),
            "latest transmitted wire intent not accepted"
        );
        let accepted = observation.accepted.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "no independently recorded accepted intent revision {}",
                snapshot.accepted_intent_revision
            )
        })?;
        ensure!(
            accepted.revision == snapshot.desired_revision,
            "desired revision mismatch"
        );
        ensure!(
            accepted == &observation.submitted,
            "latest complete intent not accepted"
        );
        let expected_video = requested_video
            .iter()
            .map(|id| {
                let participant: ParticipantId = id.parse()?;
                let peer = peers
                    .iter()
                    .find(|peer| peer.id == participant)
                    .ok_or_else(|| anyhow::anyhow!("requested video has no plan participant"))?;
                Ok(peer.track_id(TrackKind::Video, "video").as_str())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let desired_video = accepted
            .video
            .iter()
            .map(|request| request.track_id.clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            accepted.video.len() == desired_video.len(),
            "duplicate desired video track"
        );
        ensure!(
            expected_video == desired_video,
            "submitted video intent differs from plan: expected={expected_video:?}, desired={desired_video:?}"
        );
        check_mapping(
            &snapshot.video_mapping,
            &snapshot.publications,
            MediaKind::Video,
            &observation.video_receivers,
            observation.video_capacity,
        )?;
        ensure!(
            snapshot
                .video_mapping
                .values()
                .cloned()
                .collect::<BTreeSet<_>>()
                == expected_video,
            "video assignments differ from complete receive intent"
        );
        check_mapping(
            &snapshot.audio_mapping,
            &snapshot.publications,
            MediaKind::Audio,
            &observation.audio_receivers,
            observation.audio_capacity,
        )?;
        let audio = snapshot
            .audio_mapping
            .values()
            .cloned()
            .collect::<BTreeSet<_>>();
        let available_audio = snapshot
            .publications
            .values()
            .filter(|publication| publication.kind == MediaKind::Audio)
            .map(|publication| publication.id.clone())
            .collect::<BTreeSet<_>>();
        let pinned = accepted
            .audio
            .pinned
            .iter()
            .filter(|id| available_audio.contains(*id))
            .cloned()
            .collect::<BTreeSet<_>>();
        ensure!(
            pinned.len() <= observation.audio_capacity && pinned.is_subset(&audio),
            "pinned audio assignment missing"
        );
        if accepted.audio.automatic {
            let expected_automatic = peers
                .iter()
                .filter(|peer| peer.required && peer.audio)
                .map(|peer| peer.track_id(TrackKind::Audio, "audio").as_str())
                .filter(|track| !pinned.contains(track))
                .collect::<BTreeSet<_>>();
            let automatic_capacity = observation.audio_capacity - pinned.len();
            // Packet timing, power decay and immunity choose the contended subset.
            // Every live source must be assigned when it fits; otherwise live sources
            // must fill the remaining slots, independently of the observed mapping.
            ensure!(
                audio.intersection(&expected_automatic).count()
                    == expected_automatic.len().min(automatic_capacity),
                "automatic audio assignments do not cover live plan sources and available slots"
            );
        } else {
            ensure!(
                audio == pinned,
                "automatic audio assigned with automatic selection disabled"
            );
        }
        let publishers = snapshot
            .topics
            .publishers
            .iter()
            .map(|status| &status.registration)
            .collect::<Vec<_>>();
        let subscribers = snapshot
            .topics
            .subscribers
            .iter()
            .map(|status| &status.registration)
            .collect::<Vec<_>>();
        ensure!(
            publishers.len() == accepted.topics.publishers.len()
                && accepted
                    .topics
                    .publishers
                    .iter()
                    .all(|registration| publishers
                        .iter()
                        .filter(|actual| **actual == registration)
                        .count()
                        == 1),
            "topic publisher registrations differ from intent"
        );
        ensure!(
            subscribers.len() == accepted.topics.subscribers.len()
                && accepted
                    .topics
                    .subscribers
                    .iter()
                    .all(|registration| subscribers
                        .iter()
                        .filter(|actual| **actual == registration)
                        .count()
                        == 1),
            "topic subscriber registrations differ from intent"
        );
        Ok(())
    }
}

impl ControlStateOracle {
    pub fn projections_ready(observation: &Observation) -> bool {
        let control = &observation.control;
        observation.shards.len() == control.shard_generations.len()
            && observation.shards.iter().all(|shard| {
                shard.pending_generation.is_none()
                    && shard.pending_effects == 0
                    && control.shard_generations.get(shard.shard.index()) == Some(&shard.generation)
            })
    }

    pub fn check_owners(&self, observation: &Observation) -> Result<()> {
        ensure!(
            Self::projections_ready(observation),
            "owner generations have not settled"
        );
        let control = &observation.control;
        self.check_control(control)?;
        let participants = control
            .participants
            .iter()
            .map(|participant| (participant.id, participant))
            .collect::<HashMap<_, _>>();
        let tracks = control
            .tracks
            .iter()
            .map(|track| (track.meta.id, track))
            .collect::<HashMap<_, _>>();
        let mut shards = HashSet::new();
        for shard in &observation.shards {
            ensure!(shards.insert(shard.shard), "duplicate shard observation");
            let expected_participants = participants
                .values()
                .filter(|participant| participant.shard == shard.shard)
                .map(|participant| participant.id)
                .collect::<HashSet<_>>();
            let actual_participants = shard
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<HashSet<_>>();
            ensure!(
                shard.participants.len() == actual_participants.len()
                    && actual_participants == expected_participants,
                "shard participant projection differs from control on {:?}",
                shard.shard
            );
            for participant in &shard.participants {
                let owner = participants[&participant.id];
                ensure!(
                    participant.connection == owner.connection
                        && Some(participant.transport) == owner.transport,
                    "stale shard incarnation or transport"
                );
            }
            let expected_transports = shard
                .participants
                .iter()
                .map(|participant| (participant.transport, Some(participant.id)))
                .collect::<HashSet<_>>();
            ensure!(
                shard.transports.len() == expected_transports.len()
                    && shard.transports.iter().copied().collect::<HashSet<_>>()
                        == expected_transports,
                "stale or missing transport binding"
            );
            let placements = control
                .placements
                .iter()
                .filter(|placement| placement.address.shard() == shard.shard)
                .collect::<Vec<_>>();
            let resident = placements
                .iter()
                .filter(|placement| placement.resident)
                .map(|placement| placement.track)
                .collect::<HashSet<_>>();
            ensure!(
                shard.tracks.len() == resident.len()
                    && shard.tracks.iter().copied().collect::<HashSet<_>>() == resident,
                "missing or stale shard runtime"
            );
            ensure!(
                shard.plans.len() == resident.len()
                    && shard
                        .plans
                        .iter()
                        .map(|plan| plan.track)
                        .collect::<HashSet<_>>()
                        == resident.iter().map(|track| Some(*track)).collect(),
                "missing, duplicate or stale shard plan"
            );
            for plan in &shard.plans {
                let id = plan
                    .track
                    .ok_or_else(|| anyhow::anyhow!("plan references retired runtime"))?;
                let track = tracks
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("plan has no live publication"))?;
                // Topology can match self. Runtime delivery is only to bound, non-self peers.
                let bindings = track
                    .active
                    .iter()
                    .filter(|id| {
                        **id != track.meta.origin
                            && participants
                                .get(id)
                                .is_some_and(|participant| participant.transport.is_some())
                    })
                    .copied()
                    .collect::<HashSet<_>>();
                let expected_local = bindings
                    .iter()
                    .filter(|participant| {
                        participants
                            .get(participant)
                            .is_some_and(|participant| participant.shard == shard.shard)
                    })
                    .map(|participant| Some(*participant))
                    .collect::<HashSet<_>>();
                ensure!(
                    plan.local.len() == expected_local.len()
                        && plan.local.iter().copied().collect::<HashSet<_>>() == expected_local,
                    "missing, duplicate or stale local binding: track={id:?} shard={:?} expected={expected_local:?} actual={:?}",
                    shard.shard,
                    plan.local
                );
                let expected_remote = if shard.shard == track.meta.shard_id {
                    control
                        .placements
                        .iter()
                        .filter(|placement| {
                            placement.track == id
                                && placement.resident
                                && placement.address.shard() != shard.shard
                                && bindings.iter().any(|id| {
                                    participants.get(id).is_some_and(|participant| {
                                        participant.shard == placement.address.shard()
                                    })
                                })
                        })
                        .map(|placement| placement.address)
                        .collect::<HashSet<_>>()
                } else {
                    HashSet::new()
                };
                ensure!(
                    plan.remote.len() == expected_remote.len()
                        && plan.remote.iter().copied().collect::<HashSet<_>>() == expected_remote,
                    "missing, duplicate or stale remote route"
                );
                let expected_reverse = control
                    .placements
                    .iter()
                    .find(|placement| {
                        placement.track == id
                            && placement.route_installed
                            && placement.address.shard() == track.meta.shard_id
                    })
                    .map(|placement| placement.address);
                ensure!(
                    plan.reverse == expected_reverse,
                    "missing or stale reverse route/epoch"
                );
            }
            let expected_routes = placements
                .iter()
                .filter(|placement| placement.route_installed)
                .map(|placement| {
                    (
                        placement.address,
                        Some(placement.track),
                        tracks
                            .get(&placement.track)
                            .is_some_and(|track| track.meta.shard_id == shard.shard),
                    )
                })
                .collect::<HashSet<_>>();
            let actual_routes = shard
                .routes
                .iter()
                .map(|route| (route.address, route.track, route.reverse))
                .collect::<HashSet<_>>();
            ensure!(
                shard.routes.len() == expected_routes.len() && actual_routes == expected_routes,
                "missing, duplicate or stale route/epoch"
            );
        }
        Ok(())
    }

    fn check_control(&self, control: &ControlObservation) -> Result<()> {
        let allowed = self
            .participants
            .iter()
            .map(|participant| (participant.id, participant))
            .collect::<HashMap<_, _>>();
        let actual = control
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<HashSet<_>>();
        ensure!(
            actual.len() == control.participants.len(),
            "duplicate control participant"
        );
        for participant in &self.participants {
            ensure!(
                !participant.required || actual.contains(&participant.id),
                "missing live control participant {}",
                participant.id
            );
        }
        for participant in &control.participants {
            let expected = allowed.get(&participant.id).ok_or_else(|| {
                anyhow::anyhow!("stale or unknown control participant {}", participant.id)
            })?;
            ensure!(
                participant.room == expected.room_id()?,
                "control participant crossed rooms"
            );
            ensure!(
                Some(participant.connection) == expected.connection,
                "stale control connection generation"
            );
            ensure!(
                participant
                    .transport
                    .is_some_and(|transport| transport.shard() == participant.shard),
                "control owner/transport mismatch"
            );
        }
        let expected_tracks = self
            .participants
            .iter()
            .flat_map(|participant| {
                participant
                    .tracks()
                    .map(move |(id, label)| (id, (participant, label)))
            })
            .collect::<HashMap<_, _>>();
        let track_ids = control
            .tracks
            .iter()
            .map(|track| track.meta.id)
            .collect::<HashSet<_>>();
        ensure!(
            control.tracks.len() == track_ids.len(),
            "duplicate publication"
        );
        for (id, (participant, _)) in &expected_tracks {
            ensure!(
                !participant.required || track_ids.contains(id),
                "missing required publication {id}"
            );
        }
        for track in &control.tracks {
            let (expected, label) = expected_tracks
                .get(&track.meta.id)
                .ok_or_else(|| anyhow::anyhow!("stale or invented publication {:?}", track.meta))?;
            ensure!(
                track.meta.origin == expected.id
                    && Some(track.meta.connection_id) == expected.connection
                    && track.meta.room_id == expected.room_id()?
                    && track.meta.label.as_ref() == Some(label),
                "publication owner, room, kind or label mismatch"
            );
            ensure!(
                control
                    .participants
                    .iter()
                    .any(|participant| participant.id == expected.id
                        && participant.shard == track.meta.shard_id),
                "publication has no current owner"
            );
            ensure!(
                control
                    .placements
                    .iter()
                    .any(|placement| placement.track == track.meta.id
                        && placement.address.shard() == track.meta.shard_id
                        && placement.resident),
                "publication lost its origin runtime placement"
            );
            for subscriber in &track.active {
                let shard = control
                    .participants
                    .iter()
                    .find(|participant| participant.id == *subscriber)
                    .map(|participant| participant.shard)
                    .ok_or_else(|| anyhow::anyhow!("active subscription has no owner"))?;
                ensure!(
                    control
                        .placements
                        .iter()
                        .any(|placement| placement.track == track.meta.id
                            && placement.address.shard() == shard
                            && placement.resident
                            && (shard == track.meta.shard_id || placement.route_installed)),
                    "active subscription lost its destination placement/route"
                );
            }
            let candidates = track.candidates.iter().copied().collect::<HashSet<_>>();
            let active = track.active.iter().copied().collect::<HashSet<_>>();
            ensure!(
                candidates.len() == track.candidates.len()
                    && active.len() == track.active.len()
                    && active.is_subset(&candidates),
                "duplicate or unowned subscription binding"
            );
            let expected_candidates = control
                .participants
                .iter()
                .filter(|participant| participant.room == track.meta.room_id)
                .filter(|participant| {
                    if track.meta.id.kind() != TrackKind::Data {
                        return true;
                    }
                    allowed[&participant.id]
                        .topics
                        .subscribers
                        .iter()
                        .any(|subscription| {
                            let lane = if subscription.mode == TopicMode::Latest {
                                "rt"
                            } else {
                                "rel"
                            };
                            track.meta.label.as_deref()
                                == Some(format!("v1/{lane}/{}", subscription.topic).as_str())
                                && subscription.publisher_id.as_ref().is_none_or(|publisher| {
                                    *publisher == track.meta.origin.as_str()
                                })
                        })
                })
                .map(|participant| participant.id)
                .collect::<HashSet<_>>();
            ensure!(
                candidates == expected_candidates,
                "candidate membership differs from plan selector semantics"
            );
            if track.meta.id.kind() == TrackKind::Video {
                ensure!(
                    active.iter().all(|subscriber| allowed[subscriber]
                        .video_targets
                        .contains(&track.meta.id)),
                    "video binding contradicts complete receive intent"
                );
            } else {
                ensure!(
                    active == expected_candidates,
                    "all-match audio/topic binding missing or duplicated"
                );
            }
            for subscriber in candidates {
                ensure!(
                    allowed
                        .get(&subscriber)
                        .is_some_and(|subscriber| subscriber.room == expected.room)
                        && actual.contains(&subscriber),
                    "stale or cross-room subscriber"
                );
            }
        }
        let mut expected_subscriptions = HashSet::new();
        for participant in &control.participants {
            for kind in [TrackKind::Audio, TrackKind::Video] {
                expected_subscriptions.insert((
                    participant.room,
                    participant.id,
                    SelectorObservation {
                        track: None,
                        publisher: None,
                        kind: Some(kind),
                        label: None,
                    },
                    kind == TrackKind::Video,
                ));
            }
            for subscription in &allowed[&participant.id].topics.subscribers {
                let lane = if subscription.mode == TopicMode::Latest {
                    "rt"
                } else {
                    "rel"
                };
                expected_subscriptions.insert((
                    participant.room,
                    participant.id,
                    SelectorObservation {
                        track: None,
                        publisher: subscription
                            .publisher_id
                            .as_ref()
                            .map(|id| id.parse().expect("admission identity")),
                        kind: Some(TrackKind::Data),
                        label: Some(format!("v1/{lane}/{}", subscription.topic)),
                    },
                    false,
                ));
            }
        }
        let mut subscriptions = std::collections::HashSet::new();
        for subscription in &control.subscriptions {
            ensure!(
                subscriptions.insert((
                    subscription.room,
                    subscription.subscriber,
                    subscription.selector.clone(),
                    subscription.selection == SelectionPolicy::Allocated
                )),
                "duplicate pending subscription"
            );
            ensure!(
                allowed
                    .get(&subscription.subscriber)
                    .is_some_and(|subscriber| subscriber
                        .room_id()
                        .is_ok_and(|room| room == subscription.room))
                    && actual.contains(&subscription.subscriber),
                "pending subscription outlived its owner"
            );
        }
        ensure!(
            subscriptions == expected_subscriptions,
            "pending or installed selectors differ from complete plan intent"
        );
        let mut placements = HashSet::new();
        for placement in &control.placements {
            ensure!(
                placements.insert((placement.track, placement.address.shard()))
                    && track_ids.contains(&placement.track),
                "duplicate or stale placement"
            );
            ensure!(
                !placement.route_installed || placement.resident,
                "route without resident runtime"
            );
        }
        Ok(())
    }
}

fn check_mapping(
    mapping: &BTreeMap<u32, String>,
    publications: &BTreeMap<String, Publication>,
    kind: MediaKind,
    receivers: &BTreeSet<u32>,
    capacity: usize,
) -> Result<()> {
    let tracks = mapping.values().collect::<BTreeSet<_>>();
    ensure!(
        tracks.len() == mapping.len(),
        "duplicate receiver assignment"
    );
    ensure!(
        receivers.len() == capacity
            && mapping.len() <= capacity
            && mapping.keys().all(|index| receivers.contains(index)),
        "receiver exceeds negotiated capacity"
    );
    for track in tracks {
        ensure!(
            publications
                .get(track)
                .is_some_and(|publication| publication.kind == kind),
            "assignment has stale track or wrong kind: {track}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
