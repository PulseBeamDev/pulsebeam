use super::client::ClientObservation;
use anyhow::{Result, ensure};
use pulsebeam::entity::{ConnectionId, ParticipantId, PublicationKey, RoomId, TrackKind};
use pulsebeam_agent_native::agent_core::{
    ConnectionState, MediaKind, Publication, TopicRegistrations,
};
use std::collections::{BTreeMap, BTreeSet};

/// Admission and public catalog descriptors bind opaque identities. Kind,
/// label, ownership and receive intent remain plan-derived; media origin is
/// independently authenticated from actual encoded bytes.
#[derive(Clone, Debug)]
pub struct ExpectedParticipant {
    pub id: ParticipantId,
    pub room: &'static str,
    pub external_id: String,
    pub video: bool,
    pub video_label: String,
    pub audio: bool,
    pub audio_pinned: Vec<String>,
    pub automatic_audio: bool,
    pub required: bool,
    pub connection: Option<ConnectionId>,
    pub topics: TopicRegistrations,
    pub media: Vec<Publication>,
}

impl ExpectedParticipant {
    pub fn room_id(&self) -> Result<RoomId> {
        Ok(RoomId::new(self.room)?)
    }

    pub fn bind_media(&mut self, catalog: &[Publication]) -> Result<bool> {
        self.media.clear();
        for (enabled, kind, label) in [
            (self.video, MediaKind::Video, self.video_label.as_str()),
            (self.audio, MediaKind::Audio, "audio"),
        ] {
            if !enabled {
                continue;
            }
            let mut descriptors = catalog.iter().filter(|publication| {
                publication.participant_id == self.id.as_str()
                    && publication.kind == kind
                    && publication.track_id == label
            });
            let Some(descriptor) = descriptors.next() else {
                return Ok(false);
            };
            ensure!(
                descriptors.next().is_none(),
                "duplicate publisher descriptor for {kind:?}/{label}"
            );
            descriptor.id.parse::<pulsebeam::entity::PublicationId>()?;
            self.media.push(Publication {
                id: descriptor.id.clone(),
                participant_id: self.id.as_str().to_owned(),
                kind,
                track_id: label.into(),
            });
        }
        Ok(true)
    }

    pub fn publication_id(&self, kind: TrackKind, label: &str) -> PublicationKey {
        let id = self
            .media
            .iter()
            .find(|publication| {
                publication.track_id == label
                    && matches!(
                        (kind, publication.kind),
                        (TrackKind::Video, MediaKind::Video) | (TrackKind::Audio, MediaKind::Audio)
                    )
            })
            .expect("bound publisher descriptor")
            .id
            .parse()
            .expect("canonical publication");
        PublicationKey::new(kind, id)
    }

    pub fn publications(&self) -> impl Iterator<Item = Publication> + '_ {
        self.media.iter().cloned()
    }
}

pub struct ApplicationStateOracle {
    pub participants: Vec<ExpectedParticipant>,
}

impl ApplicationStateOracle {
    /// Safety within a coherent public value, without requiring pending requests to finish.
    pub fn check_client_safety(
        observation: &ClientObservation,
        allowed_origins: &BTreeSet<String>,
    ) -> Result<()> {
        let snapshot = &observation.snapshot;
        ensure!(
            snapshot
                .participants
                .keys()
                .all(|id| allowed_origins.contains(id)),
            "foreign or unadmitted participant in catalog"
        );
        ensure!(
            snapshot
                .publications
                .values()
                .all(|p| allowed_origins.contains(&p.participant_id)),
            "foreign or unadmitted publication origin"
        );
        check_mapping(
            &snapshot.video_mapping,
            &snapshot.publications,
            MediaKind::Video,
            Some(observation.video_capacity),
            Some((observation, allowed_origins)),
        )?;
        check_mapping(
            &snapshot.audio_mapping,
            &snapshot.publications,
            MediaKind::Audio,
            None,
            Some((observation, allowed_origins)),
        )?;
        Ok(())
    }

    pub fn check_client(
        &self,
        observer: &ExpectedParticipant,
        observation: &ClientObservation,
        requested_video: &[String],
    ) -> Result<()> {
        let snapshot = &observation.snapshot;
        ensure!(
            snapshot.connection == ConnectionState::Connected,
            "required application client is not connected"
        );
        ensure!(
            snapshot.participant_id.as_deref() == Some(observer.id.as_str()),
            "wrong observer identity"
        );
        ensure!(
            snapshot.room_id.as_deref() == Some(observer.room),
            "wrong observer room"
        );
        let peers = self
            .participants
            .iter()
            .filter(|peer| peer.id != observer.id && peer.room == observer.room)
            .collect::<Vec<_>>();
        let allowed = peers
            .iter()
            .map(|peer| peer.id.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let required = peers
            .iter()
            .filter(|peer| peer.required)
            .map(|peer| peer.id.as_str().to_owned())
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
        let expected_video = requested_video
            .iter()
            .map(|id| {
                let peer = self
                    .participants
                    .iter()
                    .find(|peer| peer.id.as_str() == *id && peer.video)
                    .ok_or_else(|| anyhow::anyhow!("requested video has no plan publisher"))?;
                ensure!(
                    peer.media
                        .iter()
                        .any(|publication| publication.kind == MediaKind::Video
                            && publication.track_id == peer.video_label),
                    "requested video has no current publisher descriptor"
                );
                Ok(peer
                    .publication_id(TrackKind::Video, &peer.video_label)
                    .as_str())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let allowed_video_origins = requested_video.iter().cloned().collect();
        check_mapping(
            &snapshot.video_mapping,
            &snapshot.publications,
            MediaKind::Video,
            Some(observation.video_capacity),
            Some((observation, &allowed_video_origins)),
        )?;
        ensure!(
            snapshot
                .video_mapping
                .values()
                .cloned()
                .collect::<BTreeSet<_>>()
                == expected_video,
            "video assignments differ from complete receive intent: expected {expected_video:?}, slots {:?}",
            snapshot.video_mapping
        );
        check_mapping(
            &snapshot.audio_mapping,
            &snapshot.publications,
            MediaKind::Audio,
            None,
            None,
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
        let pinned = observer
            .audio_pinned
            .iter()
            .filter_map(|id| {
                peers
                    .iter()
                    .find(|peer| peer.id.as_str() == *id && peer.audio)
            })
            .map(|peer| peer.publication_id(TrackKind::Audio, "audio").as_str())
            .filter(|id| available_audio.contains(id))
            .collect::<BTreeSet<_>>();
        ensure!(pinned.is_subset(&audio), "pinned audio assignment missing");
        if observer.automatic_audio {
            let expected_automatic = peers
                .iter()
                .filter(|peer| peer.required && peer.audio)
                .map(|peer| peer.publication_id(TrackKind::Audio, "audio").as_str())
                .filter(|track| !pinned.contains(track))
                .collect::<BTreeSet<_>>();
            ensure!(
                expected_automatic.is_subset(&audio),
                "automatic audio assignments do not cover every live plan source"
            );
        } else {
            ensure!(
                audio == pinned,
                "automatic audio assigned with automatic selection disabled"
            );
        }
        use pulsebeam_agent_native::agent_core::ffi::PublicationExecution;

        ensure!(
            observation.data_publishers.keys().collect::<BTreeSet<_>>()
                == observer.topics.publishers.iter().collect(),
            "data publisher handles differ from intent"
        );
        ensure!(
            observation.data_subscribers.keys().collect::<BTreeSet<_>>()
                == observer.topics.subscribers.iter().collect(),
            "data subscriber handles differ from intent"
        );
        for (publisher, state) in &observation.data_publishers {
            ensure!(
                state.execution == PublicationExecution::Installed
                    && state.track_id == publisher.topic
                    && state.delivery == publisher.mode.into(),
                "data publisher {publisher:?} is not installed as declared: {state:?}"
            );
        }
        for (subscriber, state) in &observation.data_subscribers {
            ensure!(
                state.selector.as_ref().is_some_and(|selector| {
                    selector.room == subscriber.room
                        && selector.participant_id == subscriber.participant_id
                        && selector.track_id == subscriber.topic
                }) && state.delivery == Some(subscriber.mode.into()),
                "wrong DATA subscription selector for {subscriber:?}: {state:?}"
            );
            let registration = pulsebeam_agent_native::agent_core::TopicPublisher {
                topic: subscriber.topic.clone(),
                mode: subscriber.mode,
            };
            let peer = peers.iter().find(|peer| {
                peer.room == subscriber.room
                    && peer.external_id == subscriber.participant_id
                    && peer.topics.publishers.contains(&registration)
            });
            let catalog_id = peer.and_then(|peer| {
                observation
                    .data_catalog
                    .get(&(peer.external_id.clone(), registration))
            });
            if peer.is_some_and(|peer| peer.required) {
                ensure!(
                    catalog_id.is_some() && state.execution == PublicationExecution::Installed,
                    "data source not installed for {subscriber:?}: {state:?}"
                );
            }
            ensure!(
                state
                    .source_track_id
                    .as_ref()
                    .is_none_or(|id| Some(id) == catalog_id)
                    && (state.execution != PublicationExecution::Installed
                        || state.source_track_id.is_some()),
                "wrong data binding for {subscriber:?}: {state:?}"
            );
        }
        Ok(())
    }
}

fn check_mapping(
    mapping: &BTreeMap<u32, String>,
    catalog: &BTreeMap<String, Publication>,
    kind: MediaKind,
    capacity: Option<usize>,
    live: Option<(&ClientObservation, &BTreeSet<String>)>,
) -> Result<()> {
    ensure!(
        capacity.is_none_or(|capacity| mapping.len() <= capacity),
        "receiver exceeds requested capacity"
    );
    let mut tracks = BTreeSet::new();
    for (index, id) in mapping {
        ensure!(tracks.insert(id), "duplicate media assignment");
        let publication = catalog
            .get(id)
            .or_else(|| {
                let (observation, allowed) = live?;
                observation
                    .owned_receiver_publication(*index, id)
                    .filter(|publication| allowed.contains(&publication.participant_id))
            })
            .ok_or_else(|| anyhow::anyhow!("assignment references missing publication {id}"))?;
        ensure!(publication.kind == kind, "assignment has wrong media kind");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
