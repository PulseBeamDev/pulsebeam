use super::client::ClientObservation;
use anyhow::{Result, ensure};
use pulsebeam::entity::{ConnectionId, ParticipantId, RoomExternalId, RoomId, TrackId, TrackKind};
use pulsebeam_agent_native::agent_core::{
    ConnectionState, MediaKind, Publication, TopicRegistrations,
};
use pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
use std::collections::{BTreeMap, BTreeSet};

/// Application declarations bound to the producer's public admission. Expected
/// media identities never come from a receiver or its decoded output.
#[derive(Clone, Debug)]
pub struct ExpectedParticipant {
    pub id: ParticipantId,
    pub room: &'static str,
    pub video: bool,
    pub video_label: String,
    pub audio: bool,
    pub audio_pinned: Vec<String>,
    pub automatic_audio: bool,
    pub required: bool,
    pub connection: Option<ConnectionId>,
    pub topics: TopicRegistrations,
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

    fn publications(&self) -> impl Iterator<Item = Publication> + '_ {
        [
            (
                self.video,
                TrackKind::Video,
                MediaKind::Video,
                self.video_label.as_str(),
            ),
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
            observation.video_capacity,
        )?;
        check_mapping(
            &snapshot.audio_mapping,
            &snapshot.publications,
            MediaKind::Audio,
            observation.audio_capacity,
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
        let expected_video = requested_video
            .iter()
            .map(|id| {
                let peer = peers
                    .iter()
                    .find(|peer| peer.id.as_str() == *id && peer.video)
                    .ok_or_else(|| anyhow::anyhow!("requested video has no plan publisher"))?;
                Ok(peer.track_id(TrackKind::Video, &peer.video_label).as_str())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        check_mapping(
            &snapshot.video_mapping,
            &snapshot.publications,
            MediaKind::Video,
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
        let pinned = observer
            .audio_pinned
            .iter()
            .filter_map(|id| {
                peers
                    .iter()
                    .find(|peer| peer.id.as_str() == *id && peer.audio)
            })
            .map(|peer| peer.track_id(TrackKind::Audio, "audio").as_str())
            .filter(|id| available_audio.contains(id))
            .collect::<BTreeSet<_>>();
        ensure!(
            pinned.len() <= observation.audio_capacity && pinned.is_subset(&audio),
            "pinned audio assignment missing"
        );
        if observer.automatic_audio {
            let expected_automatic = peers
                .iter()
                .filter(|peer| peer.required && peer.audio)
                .map(|peer| peer.track_id(TrackKind::Audio, "audio").as_str())
                .filter(|track| !pinned.contains(track))
                .collect::<BTreeSet<_>>();
            let automatic_capacity = observation.audio_capacity - pinned.len();
            // Timing chooses the contended subset, not whether available slots are filled.
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
            publishers.len() == observer.topics.publishers.len()
                && observer
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
            subscribers.len() == observer.topics.subscribers.len()
                && observer
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
        let mut data_ids = BTreeSet::new();
        for publisher in &snapshot.topics.publishers {
            let id = publisher
                .data_track_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("publisher has no public data binding"))?;
            ensure!(data_ids.insert(id), "duplicate publisher data binding");
        }
        for subscriber in &snapshot.topics.subscribers {
            let publishing = peers
                .iter()
                .filter(|peer| {
                    peer.topics.publishers.iter().any(|publisher| {
                        publisher.topic == subscriber.registration.topic
                            && publisher.mode == subscriber.registration.mode
                    })
                })
                .collect::<Vec<_>>();
            let required = publishing.iter().filter(|peer| peer.required).count();
            ensure!(
                (required..=publishing.len()).contains(&subscriber.publishers),
                "subscriber bindings differ from declared room publishers: required={required}, allowed={}, actual={}",
                publishing.len(),
                subscriber.publishers
            );
        }
        Ok(())
    }
}

fn check_mapping(
    mapping: &BTreeMap<u32, String>,
    catalog: &BTreeMap<String, Publication>,
    kind: MediaKind,
    capacity: usize,
) -> Result<()> {
    ensure!(
        mapping.len() <= capacity,
        "receiver exceeds requested capacity"
    );
    let mut tracks = BTreeSet::new();
    for id in mapping.values() {
        ensure!(tracks.insert(id), "duplicate media assignment");
        let publication = catalog
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("assignment references missing publication {id}"))?;
        ensure!(publication.kind == kind, "assignment has wrong media kind");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
