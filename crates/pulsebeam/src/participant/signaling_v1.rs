use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_proto::signaling_v1::{LocalTrack, SendIntent, TrackKind as WireTrackKind};

use crate::entity::TrackKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendBindingError {
    RelabeledSender,
    MovedLabel,
}

#[derive(Default)]
pub(crate) struct SenderBindings {
    by_index: HashMap<u32, (TrackKind, String)>,
    by_label: HashMap<(TrackKind, String), u32>,
}

pub(crate) struct SendPlan {
    pub(crate) tracks: Vec<LocalTrack>,
    bindings: SenderBindings,
}

impl SenderBindings {
    /// Reconcile the entire send section without mutating established bindings.
    /// Only a successfully accepted complete Intent may commit the returned plan.
    pub(crate) fn plan(
        &self,
        intent: Option<&SendIntent>,
        negotiated: &HashMap<u32, TrackKind>,
    ) -> Result<SendPlan, SendBindingError> {
        let mut bindings = Self {
            by_index: self.by_index.clone(),
            by_label: self.by_label.clone(),
        };
        let mut tracks = Vec::new();
        let mut used_indices = HashSet::new();
        let mut used_labels = HashSet::new();
        for track in intent.into_iter().flat_map(|send| &send.tracks) {
            let kind = match WireTrackKind::try_from(track.kind) {
                Ok(WireTrackKind::Audio) => TrackKind::Audio,
                Ok(WireTrackKind::Video) => TrackKind::Video,
                _ => continue,
            };
            if track.label.is_empty()
                || track.label.len() > 64
                || negotiated.get(&track.sender_index) != Some(&kind)
            {
                continue;
            }
            let key = (kind, track.label.clone());
            if used_indices.contains(&track.sender_index) || used_labels.contains(&key) {
                continue;
            }
            if bindings
                .by_index
                .get(&track.sender_index)
                .is_some_and(|previous| previous != &key)
            {
                return Err(SendBindingError::RelabeledSender);
            }
            if bindings
                .by_label
                .get(&key)
                .is_some_and(|previous| previous != &track.sender_index)
            {
                return Err(SendBindingError::MovedLabel);
            }
            used_indices.insert(track.sender_index);
            used_labels.insert(key.clone());
            bindings.by_index.insert(track.sender_index, key.clone());
            bindings.by_label.insert(key, track.sender_index);
            tracks.push(track.clone());
        }
        Ok(SendPlan { tracks, bindings })
    }

    pub(crate) fn commit(&mut self, plan: SendPlan) -> Vec<LocalTrack> {
        *self = plan.bindings;
        plan.tracks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputError {
    Decode(pulsebeam_proto::codec::DecodeError),
    Intent {
        revision: u64,
        cause: IntentError,
    },
    Binding {
        revision: u64,
        cause: super::upstream::SenderLabelError,
    },
}

impl InputError {
    pub(crate) fn response(self) -> pulsebeam_proto::signaling_v1::ServerMessage {
        use pulsebeam_proto::signaling_v1::{Error, ErrorCode, ServerMessage, server_message};
        let (code, message, intent_revision) = match self {
            Self::Decode(_) => (
                ErrorCode::InvalidMessage,
                "invalid signaling envelope",
                None,
            ),
            Self::Intent { revision, cause } => (
                ErrorCode::ProtocolError,
                match cause {
                    IntentError::VideoReceiverCapacity => "video receiver capacity exceeded",
                    IntentError::SendBinding(_) => "sender binding changed",
                },
                Some(revision),
            ),
            Self::Binding { revision, .. } => (
                ErrorCode::ProtocolError,
                "sender binding unavailable",
                Some(revision),
            ),
        };
        ServerMessage {
            payload: Some(server_message::Payload::Error(Error {
                code: code.into(),
                message: message.into(),
                fatal: true,
                intent_revision,
            })),
        }
    }
}

pub(crate) enum NativeInput {
    Intent(IntentDecision),
    RenewAuthorization(String),
}

pub(crate) enum AppliedInput {
    Intent {
        published: Vec<LocalTrack>,
        receive: Option<pulsebeam_proto::signaling_v1::ReceiveIntent>,
    },
    Replay,
    RenewAuthorization(String),
}

pub(crate) fn decode_input(
    bytes: &[u8],
    state: &IntentState,
    negotiated_senders: &HashMap<u32, TrackKind>,
    video_receiver_capacity: usize,
) -> Result<NativeInput, InputError> {
    use pulsebeam_proto::signaling_v1::client_message::Payload;
    let message = pulsebeam_proto::codec::decode_client(bytes).map_err(InputError::Decode)?;
    match message.payload {
        Some(Payload::Intent(intent)) => state
            .plan(&intent, negotiated_senders, video_receiver_capacity)
            .map(NativeInput::Intent)
            .map_err(|cause| InputError::Intent {
                revision: intent.revision,
                cause,
            }),
        Some(Payload::RenewAuthorization(renewal)) => {
            Ok(NativeInput::RenewAuthorization(renewal.token))
        }
        None => Err(InputError::Decode(
            pulsebeam_proto::codec::DecodeError::MissingPayload,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntentError {
    VideoReceiverCapacity,
    SendBinding(SendBindingError),
}

#[derive(Default)]
pub(crate) struct IntentState {
    revision: u64,
    senders: SenderBindings,
    receive: Option<pulsebeam_proto::signaling_v1::ReceiveIntent>,
}

pub(crate) enum IntentDecision {
    Replay,
    Accept(Box<IntentPlan>),
}

pub(crate) struct IntentPlan {
    revision: u64,
    send: SendPlan,
    receive: Option<pulsebeam_proto::signaling_v1::ReceiveIntent>,
}

impl IntentState {
    pub(crate) fn plan(
        &self,
        intent: &pulsebeam_proto::signaling_v1::Intent,
        negotiated_senders: &HashMap<u32, TrackKind>,
        video_receiver_capacity: usize,
    ) -> Result<IntentDecision, IntentError> {
        if intent.revision == 0 || intent.revision <= self.revision {
            return Ok(IntentDecision::Replay);
        }
        let mut distinct_video = HashSet::new();
        for track in intent
            .receive
            .as_ref()
            .and_then(|receive| receive.video.as_ref())
            .into_iter()
            .flat_map(|video| &video.tracks)
        {
            if !track.track_id.is_empty() && track.track_id.len() <= 128 {
                distinct_video.insert(&track.track_id);
            }
        }
        if distinct_video.len() > video_receiver_capacity {
            return Err(IntentError::VideoReceiverCapacity);
        }
        let send = self
            .senders
            .plan(intent.send.as_ref(), negotiated_senders)
            .map_err(IntentError::SendBinding)?;
        let mut receive = intent.receive.clone();
        if let Some(receive) = &mut receive {
            if let Some(video) = &mut receive.video {
                video
                    .tracks
                    .retain(|track| !track.track_id.is_empty() && track.track_id.len() <= 128);
            }
            if let Some(audio) = &mut receive.audio {
                audio
                    .tracks
                    .retain(|track| !track.track_id.is_empty() && track.track_id.len() <= 128);
            }
        }
        Ok(IntentDecision::Accept(Box::new(IntentPlan {
            revision: intent.revision,
            send,
            receive,
        })))
    }

    pub(crate) fn commit(
        &mut self,
        decision: IntentPlan,
    ) -> (
        Vec<LocalTrack>,
        Option<pulsebeam_proto::signaling_v1::ReceiveIntent>,
    ) {
        self.revision = decision.revision;
        self.receive = decision.receive;
        let published = self.senders.commit(decision.send);
        (published, self.receive.clone())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogError {
    MissingIdentity,
    DuplicateIdentity,
    MissingPublisher,
    ChangedIdentity,
    OversizedSnapshot,
    RevisionExhausted,
}

#[derive(Default)]
pub(crate) struct CatalogState {
    revision: u64,
    participants: HashMap<String, String>,
    tracks: HashMap<String, pulsebeam_proto::signaling_v1::RemoteTrack>,
    known_participants: HashMap<String, String>,
    known_tracks: HashMap<String, pulsebeam_proto::signaling_v1::RemoteTrack>,
}

pub(crate) struct CatalogPlan {
    pub(crate) message: pulsebeam_proto::signaling_v1::ServerMessage,
    next: CatalogState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappingError {
    UnknownTrack,
    WrongKind,
    DuplicateReceiver,
    DuplicateTrack,
}

impl CatalogState {
    pub(crate) fn mapping(
        &self,
        intent_revision: u64,
        video: impl IntoIterator<Item = (u32, crate::entity::TrackId)>,
        audio: impl IntoIterator<Item = (u32, crate::entity::TrackId)>,
    ) -> Result<pulsebeam_proto::signaling_v1::Mapping, MappingError> {
        use pulsebeam_proto::signaling_v1::{Mapping, TrackKind, TrackMapping, TrackMappings};
        let mut used_receivers = HashSet::new();
        let mut used_tracks = HashSet::new();
        let mut collect = |assignments: Vec<(u32, crate::entity::TrackId)>, kind| {
            let mut tracks = Vec::with_capacity(assignments.len());
            for (receiver_index, id) in assignments {
                let track_id = id.as_str();
                let remote = self
                    .tracks
                    .get(&track_id)
                    .ok_or(MappingError::UnknownTrack)?;
                if remote.kind != kind as i32 {
                    return Err(MappingError::WrongKind);
                }
                if !used_receivers.insert(receiver_index) {
                    return Err(MappingError::DuplicateReceiver);
                }
                if !used_tracks.insert(track_id.clone()) {
                    return Err(MappingError::DuplicateTrack);
                }
                tracks.push(TrackMapping {
                    receiver_index,
                    track_id,
                });
            }
            tracks.sort_by_key(|track| track.receiver_index);
            Ok(TrackMappings { tracks })
        };
        let video = collect(video.into_iter().collect(), TrackKind::Video)?;
        let audio = collect(audio.into_iter().collect(), TrackKind::Audio)?;
        Ok(Mapping {
            intent_revision,
            video: Some(video),
            audio: Some(audio),
        })
    }

    pub(crate) fn plan(
        &self,
        peers: impl IntoIterator<Item = super::effect::RoomPeer>,
        tracks: impl IntoIterator<Item = crate::track::TrackMeta>,
        recipient: crate::entity::ParticipantId,
        recipient_external_id: &str,
    ) -> Result<Option<CatalogPlan>, CatalogError> {
        use pulsebeam_proto::signaling_v1::{
            Catalog, CatalogDelta, CatalogSnapshot, Participant, RemoteTrack, ServerMessage,
            TrackKind as WireKind, catalog, server_message,
        };

        if recipient_external_id.is_empty() || recipient_external_id.len() > 256 {
            return Err(CatalogError::MissingIdentity);
        }
        let mut participants = HashMap::new();
        let mut external_ids = HashSet::new();
        external_ids.insert(recipient_external_id.to_owned());
        for peer in peers {
            if peer.id == recipient {
                continue;
            }
            let external = peer.external_id.ok_or(CatalogError::MissingIdentity)?;
            let id = peer.id.as_str();
            let external = external.as_str().to_owned();
            if id.is_empty() || id.len() > 128 || external.is_empty() || external.len() > 256 {
                return Err(CatalogError::MissingIdentity);
            }
            if participants.insert(id, external.clone()).is_some() || !external_ids.insert(external)
            {
                return Err(CatalogError::DuplicateIdentity);
            }
        }
        let mut visible_tracks = HashMap::new();
        let mut labels = HashSet::new();
        for meta in tracks {
            if meta.origin == recipient || meta.id.kind() == TrackKind::Data {
                continue;
            }
            let publisher = meta.origin.as_str();
            if !participants.contains_key(&publisher) {
                return Err(CatalogError::MissingPublisher);
            }
            let label = meta.label.ok_or(CatalogError::MissingIdentity)?;
            let kind = match meta.id.kind() {
                TrackKind::Audio => WireKind::Audio,
                TrackKind::Video => WireKind::Video,
                TrackKind::Data => continue,
            };
            if label.is_empty() || label.len() > 64 {
                return Err(CatalogError::MissingIdentity);
            }
            if !labels.insert((publisher.clone(), kind as i32, label.clone())) {
                return Err(CatalogError::DuplicateIdentity);
            }
            let id = meta.id.as_str();
            let remote = RemoteTrack {
                track_id: id.clone(),
                participant_id: publisher,
                kind: kind as i32,
                label,
            };
            if visible_tracks.insert(id, remote).is_some() {
                return Err(CatalogError::DuplicateIdentity);
            }
        }
        for (id, external) in &participants {
            if self
                .known_participants
                .get(id)
                .is_some_and(|old| old != external)
            {
                return Err(CatalogError::ChangedIdentity);
            }
        }
        for (id, track) in &visible_tracks {
            if self.known_tracks.get(id).is_some_and(|old| old != track) {
                return Err(CatalogError::ChangedIdentity);
            }
        }
        if self.revision != 0 && self.participants == participants && self.tracks == visible_tracks
        {
            return Ok(None);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(CatalogError::RevisionExhausted)?;
        let mut snapshot = CatalogSnapshot {
            participants: participants
                .iter()
                .map(|(id, external)| Participant {
                    participant_id: id.clone(),
                    participant_external_id: external.clone(),
                })
                .collect(),
            tracks: visible_tracks.values().cloned().collect(),
        };
        snapshot
            .participants
            .sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
        snapshot.tracks.sort_by(|a, b| a.track_id.cmp(&b.track_id));
        let wrap = |state| ServerMessage {
            payload: Some(server_message::Payload::Catalog(Catalog {
                revision,
                state: Some(state),
            })),
        };
        let full = wrap(catalog::State::Snapshot(snapshot));
        pulsebeam_proto::codec::encode_server(&full)
            .map_err(|_| CatalogError::OversizedSnapshot)?;
        let message = if self.revision == 0 {
            full
        } else {
            let mut delta = CatalogDelta {
                added_participants: participants
                    .iter()
                    .filter(|(id, _)| !self.participants.contains_key(*id))
                    .map(|(id, external)| Participant {
                        participant_id: id.clone(),
                        participant_external_id: external.clone(),
                    })
                    .collect(),
                removed_participant_ids: self
                    .participants
                    .keys()
                    .filter(|id| !participants.contains_key(*id))
                    .cloned()
                    .collect(),
                added_tracks: visible_tracks
                    .iter()
                    .filter(|(id, _)| !self.tracks.contains_key(*id))
                    .map(|(_, track)| track.clone())
                    .collect(),
                removed_track_ids: self
                    .tracks
                    .iter()
                    .filter(|(id, track)| {
                        !visible_tracks.contains_key(*id)
                            && participants.contains_key(&track.participant_id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
            };
            delta
                .added_participants
                .sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
            delta.removed_participant_ids.sort();
            delta
                .added_tracks
                .sort_by(|a, b| a.track_id.cmp(&b.track_id));
            delta.removed_track_ids.sort();
            let change = wrap(catalog::State::Delta(delta));
            if pulsebeam_proto::codec::encode_server(&change).is_ok() {
                change
            } else {
                full
            }
        };
        Ok(Some(CatalogPlan {
            message,
            next: Self {
                revision,
                known_participants: self
                    .known_participants
                    .iter()
                    .chain(&participants)
                    .map(|(id, external)| (id.clone(), external.clone()))
                    .collect(),
                known_tracks: self
                    .known_tracks
                    .iter()
                    .chain(&visible_tracks)
                    .map(|(id, track)| (id.clone(), track.clone()))
                    .collect(),
                participants,
                tracks: visible_tracks,
            },
        }))
    }

    pub(crate) fn commit(&mut self, plan: CatalogPlan) {
        *self = plan.next;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputError {
    Catalog(CatalogError),
    Mapping(MappingError),
    Encode(pulsebeam_proto::codec::EncodeError),
}

pub(crate) struct NativeOutput {
    pub(crate) cid: str0m::channel::ChannelId,
    pub(crate) bytes: Vec<u8>,
}

enum PendingOutput {
    Catalog(Box<CatalogPlan>),
    Mapping(pulsebeam_proto::signaling_v1::Mapping),
}

#[derive(Default)]
pub(crate) struct NativeSession {
    pub(crate) cid: Option<str0m::channel::ChannelId>,
    pub(crate) intents: IntentState,
    catalog: CatalogState,
    peers: HashMap<crate::entity::ParticipantId, super::effect::RoomPeer>,
    mapping: Option<pulsebeam_proto::signaling_v1::Mapping>,
    pending: Option<PendingOutput>,
    dirty: bool,
    force_mapping: bool,
}

impl NativeSession {
    pub(crate) fn set_cid(&mut self, cid: str0m::channel::ChannelId) {
        self.cid = Some(cid);
        self.catalog = CatalogState::default();
        self.mapping = None;
        self.pending = None;
        self.dirty = true;
        self.force_mapping = true;
    }

    pub(crate) fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = super::effect::RoomPeer>,
        removed: impl IntoIterator<Item = crate::entity::ParticipantId>,
    ) {
        for peer in added {
            self.peers.insert(peer.id, peer);
        }
        for id in removed {
            self.peers.remove(&id);
        }
        self.dirty = true;
    }

    pub(crate) fn apply_input(
        &mut self,
        bytes: &[u8],
        negotiated_senders: &HashMap<u32, TrackKind>,
        video_receiver_capacity: usize,
        upstream: &mut super::upstream::Upstream,
    ) -> Result<AppliedInput, InputError> {
        match decode_input(
            bytes,
            &self.intents,
            negotiated_senders,
            video_receiver_capacity,
        )? {
            NativeInput::Intent(IntentDecision::Replay) => {
                self.mark_mapping_dirty();
                Ok(AppliedInput::Replay)
            }
            NativeInput::Intent(IntentDecision::Accept(plan)) => {
                let labels = plan
                    .send
                    .tracks
                    .iter()
                    .filter_map(|track| {
                        let kind =
                            match pulsebeam_proto::signaling_v1::TrackKind::try_from(track.kind) {
                                Ok(pulsebeam_proto::signaling_v1::TrackKind::Audio) => {
                                    TrackKind::Audio
                                }
                                Ok(pulsebeam_proto::signaling_v1::TrackKind::Video) => {
                                    TrackKind::Video
                                }
                                _ => return None,
                            };
                        Some((track.sender_index, kind, track.label.as_str()))
                    })
                    .collect::<Vec<_>>();
                upstream
                    .bind_sender_labels_atomically(&labels)
                    .map_err(|cause| InputError::Binding {
                        revision: plan.revision,
                        cause,
                    })?;
                let (published, receive) = self.intents.commit(*plan);
                self.mark_mapping_dirty();
                Ok(AppliedInput::Intent { published, receive })
            }
            NativeInput::RenewAuthorization(token) => Ok(AppliedInput::RenewAuthorization(token)),
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn mark_mapping_dirty(&mut self) {
        self.force_mapping = true;
        self.dirty = true;
    }

    pub(crate) fn needs_poll(&self) -> bool {
        self.cid.is_some() && self.pending.is_none() && self.dirty
    }

    pub(crate) fn poll(
        &mut self,
        publications: &[crate::track::TrackMeta],
        assignments: (
            Vec<(u32, crate::entity::TrackId)>,
            Vec<(u32, crate::entity::TrackId)>,
        ),
        recipient: crate::entity::ParticipantId,
        external_id: &str,
    ) -> Result<Option<NativeOutput>, OutputError> {
        use pulsebeam_proto::signaling_v1::{ServerMessage, server_message};
        let Some(cid) = self.cid else { return Ok(None) };
        if !self.dirty || self.pending.is_some() {
            return Ok(None);
        }
        let planned = self
            .catalog
            .plan(
                self.peers.values().cloned(),
                publications.iter().cloned(),
                recipient,
                external_id,
            )
            .map_err(OutputError::Catalog)?;
        if let Some(plan) = planned {
            if let Some(mut cleared) = self.mapping.clone() {
                for group in [&mut cleared.video, &mut cleared.audio] {
                    if let Some(group) = group.as_mut() {
                        group
                            .tracks
                            .retain(|track| plan.next.tracks.contains_key(&track.track_id));
                    }
                }
                if self.mapping.as_ref() != Some(&cleared) {
                    let message = ServerMessage {
                        payload: Some(server_message::Payload::Mapping(cleared.clone())),
                    };
                    let bytes = pulsebeam_proto::codec::encode_server(&message)
                        .map_err(OutputError::Encode)?;
                    self.pending = Some(PendingOutput::Mapping(cleared));
                    return Ok(Some(NativeOutput { cid, bytes }));
                }
            }
            let bytes = pulsebeam_proto::codec::encode_server(&plan.message)
                .map_err(OutputError::Encode)?;
            self.pending = Some(PendingOutput::Catalog(Box::new(plan)));
            return Ok(Some(NativeOutput { cid, bytes }));
        }
        let video = assignments
            .0
            .into_iter()
            .filter(|(_, id)| self.catalog.tracks.contains_key(&id.as_str()));
        let audio = assignments
            .1
            .into_iter()
            .filter(|(_, id)| self.catalog.tracks.contains_key(&id.as_str()));
        let mapping = self
            .catalog
            .mapping(self.intents.revision(), video, audio)
            .map_err(OutputError::Mapping)?;
        if !self.force_mapping && self.mapping.as_ref() == Some(&mapping) {
            self.dirty = false;
            return Ok(None);
        }
        let message = ServerMessage {
            payload: Some(server_message::Payload::Mapping(mapping.clone())),
        };
        let bytes = pulsebeam_proto::codec::encode_server(&message).map_err(OutputError::Encode)?;
        self.pending = Some(PendingOutput::Mapping(mapping));
        Ok(Some(NativeOutput { cid, bytes }))
    }

    pub(crate) fn commit_sent(&mut self) {
        match self.pending.take() {
            Some(PendingOutput::Catalog(plan)) => self.catalog.commit(*plan),
            Some(PendingOutput::Mapping(mapping)) => {
                self.mapping = Some(mapping);
                self.force_mapping = false;
            }
            None => debug_assert!(false, "native signaling commit requires pending output"),
        }
        self.dirty = true;
    }

    pub(crate) fn retry_pending(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media() -> HashMap<u32, TrackKind> {
        HashMap::from_iter([
            (0, TrackKind::Audio),
            (2, TrackKind::Video),
            (3, TrackKind::Audio),
        ])
    }

    fn track(sender_index: u32, kind: WireTrackKind, label: &str) -> LocalTrack {
        LocalTrack {
            sender_index,
            kind: kind.into(),
            label: label.to_owned(),
        }
    }

    fn peer(id: crate::entity::ParticipantId, external: &str) -> super::super::effect::RoomPeer {
        super::super::effect::RoomPeer {
            id,
            external_id: Some(crate::entity::ParticipantExternalId::new(external).unwrap()),
        }
    }

    fn remote_audio(
        publisher: crate::entity::ParticipantId,
        label: &str,
    ) -> crate::track::TrackMeta {
        let (_, mut descriptor) = crate::track::test_utils::make_audio_track(
            publisher,
            str0m::media::Mid::from("remote-mid"),
        );
        let meta = descriptor.meta_mut();
        meta.id = publisher.derive_track_id(TrackKind::Audio, label);
        meta.label = Some(label.to_owned());
        meta.clone()
    }

    #[test]
    fn native_input_decodes_compressed_intent_and_renewal_without_legacy_fallback() {
        use pulsebeam_proto::signaling_v1::{
            ClientMessage, Intent, RenewAuthorization, client_message,
        };
        let intent = ClientMessage {
            payload: Some(client_message::Payload::Intent(Intent {
                revision: 1,
                ..Default::default()
            })),
        };
        let bytes = pulsebeam_proto::codec::encode_client(&intent).unwrap();
        let state = IntentState::default();
        assert!(matches!(
            decode_input(&bytes, &state, &HashMap::new(), 0),
            Ok(NativeInput::Intent(IntentDecision::Accept(_)))
        ));
        let renewal = ClientMessage {
            payload: Some(client_message::Payload::RenewAuthorization(
                RenewAuthorization {
                    token: "token".into(),
                },
            )),
        };
        let bytes = pulsebeam_proto::codec::encode_client(&renewal).unwrap();
        assert!(matches!(
            decode_input(&bytes, &state, &HashMap::new(), 0),
            Ok(NativeInput::RenewAuthorization(jwt)) if jwt == "token"
        ));
        assert!(matches!(
            decode_input(&[0x80], &state, &HashMap::new(), 0),
            Err(InputError::Decode(_))
        ));
    }

    #[test]
    fn native_session_rejects_sender_binding_without_committing_partial_intent() {
        use pulsebeam_proto::signaling_v1::{ClientMessage, Intent, SendIntent, client_message};
        let publisher = crate::entity::ParticipantId::new();
        let room = crate::entity::RoomExternalId::new("binding-room").unwrap();
        let ctx = crate::log::LogCtx {
            room_id: crate::entity::RoomId::from_external(&room),
            participant_id: publisher,
        };
        let mut upstream = super::super::upstream::Upstream::new(ctx);
        for (index, mid) in [(0, "a"), (3, "b")] {
            let mid = str0m::media::Mid::from(mid);
            let (track, descriptor) = crate::track::test_utils::make_audio_track(publisher, mid);
            assert!(upstream.add_published_track(index, mid, track, descriptor));
        }
        let original = upstream.track_for_sender_index(0);
        upstream
            .announce_state_mut(str0m::media::Mid::from("b"), true)
            .unwrap();
        let encode = |revision, tracks| {
            pulsebeam_proto::codec::encode_client(&ClientMessage {
                payload: Some(client_message::Payload::Intent(Intent {
                    revision,
                    send: Some(SendIntent { tracks }),
                    ..Default::default()
                })),
            })
            .unwrap()
        };
        let mut session = NativeSession::default();
        let failed = session.apply_input(
            &encode(
                1,
                vec![
                    track(0, WireTrackKind::Audio, "mic"),
                    track(3, WireTrackKind::Audio, "speaker"),
                ],
            ),
            &media(),
            0,
            &mut upstream,
        );
        assert!(matches!(
            failed,
            Err(InputError::Binding { revision: 1, .. })
        ));
        assert_eq!(session.intents.revision(), 0);
        assert_eq!(upstream.track_for_sender_index(0), original);
        assert!(matches!(
            session.apply_input(
                &encode(2, vec![track(0, WireTrackKind::Audio, "mic")]),
                &media(),
                0,
                &mut upstream,
            ),
            Ok(AppliedInput::Intent { published, .. }) if published.len() == 1
        ));
        assert_eq!(session.intents.revision(), 2);
        assert_eq!(
            upstream.track_for_sender_index(0),
            Some(publisher.derive_track_id(TrackKind::Audio, "mic"))
        );
    }

    #[test]
    fn fatal_intent_error_identifies_candidate_revision_without_secret_input() {
        use pulsebeam_proto::signaling_v1::{
            ClientMessage, Intent, ReceiveIntent, VideoIntent, VideoTrackIntent, client_message,
            server_message::Payload,
        };
        let message = ClientMessage {
            payload: Some(client_message::Payload::Intent(Intent {
                revision: 9,
                receive: Some(ReceiveIntent {
                    video: Some(VideoIntent {
                        tracks: vec![VideoTrackIntent {
                            track_id: "private-track".into(),
                            options: None,
                        }],
                    }),
                    audio: None,
                }),
                ..Default::default()
            })),
        };
        let bytes = pulsebeam_proto::codec::encode_client(&message).unwrap();
        let Err(err) = decode_input(&bytes, &IntentState::default(), &HashMap::new(), 0) else {
            panic!("capacity overflow must be fatal");
        };
        assert_eq!(
            err,
            InputError::Intent {
                revision: 9,
                cause: IntentError::VideoReceiverCapacity
            }
        );
        let Payload::Error(wire) = err.response().payload.unwrap() else {
            panic!("expected error response")
        };
        assert!(wire.fatal);
        assert_eq!(wire.intent_revision, Some(9));
        assert_eq!(
            wire.code,
            pulsebeam_proto::signaling_v1::ErrorCode::ProtocolError as i32
        );
        assert!(!wire.message.contains("private-track"));
    }

    #[test]
    fn intent_revisions_replay_mapping_without_changing_accepted_state() {
        use pulsebeam_proto::signaling_v1::Intent;
        let mut state = IntentState::default();
        let initial = Intent {
            revision: 2,
            send: Some(SendIntent {
                tracks: vec![track(0, WireTrackKind::Audio, "mic")],
            }),
            ..Default::default()
        };
        let IntentDecision::Accept(plan) = state.plan(&initial, &media(), 0).unwrap() else {
            panic!("fresh revision must be accepted");
        };
        assert_eq!(state.commit(*plan).0.len(), 1);
        assert_eq!(state.revision(), 2);
        let conflicting = Intent {
            revision: 2,
            send: Some(SendIntent {
                tracks: vec![track(0, WireTrackKind::Audio, "different")],
            }),
            ..Default::default()
        };
        assert!(matches!(
            state.plan(&conflicting, &media(), 0),
            Ok(IntentDecision::Replay)
        ));
        assert!(matches!(
            state.plan(
                &Intent {
                    revision: 0,
                    ..Default::default()
                },
                &media(),
                0
            ),
            Ok(IntentDecision::Replay)
        ));
        assert_eq!(state.revision(), 2);
    }

    #[test]
    fn video_capacity_error_is_fatal_and_does_not_bind_senders() {
        use pulsebeam_proto::signaling_v1::{Intent, ReceiveIntent, VideoIntent, VideoTrackIntent};
        let mut state = IntentState::default();
        let invalid = Intent {
            revision: 1,
            send: Some(SendIntent {
                tracks: vec![track(0, WireTrackKind::Audio, "mic")],
            }),
            receive: Some(ReceiveIntent {
                video: Some(VideoIntent {
                    tracks: ["first", "first", "second"]
                        .into_iter()
                        .map(|track_id| VideoTrackIntent {
                            track_id: track_id.to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                }),
                ..Default::default()
            }),
        };
        assert!(matches!(
            state.plan(&invalid, &media(), 1),
            Err(IntentError::VideoReceiverCapacity)
        ));
        assert_eq!(state.revision(), 0);
        let valid = Intent {
            revision: 2,
            receive: None,
            ..invalid
        };
        let IntentDecision::Accept(plan) = state.plan(&valid, &media(), 1).unwrap() else {
            panic!("failed candidate must not reserve sender labels");
        };
        assert_eq!(state.commit(*plan).0.len(), 1);
    }

    #[test]
    fn receive_ids_are_bounded_before_retention() {
        use pulsebeam_proto::signaling_v1::{
            AudioIntent, AudioTrackIntent, Intent, ReceiveIntent, VideoIntent, VideoTrackIntent,
        };
        let ids = [String::new(), "x".repeat(129), "é".repeat(64)];
        let intent = Intent {
            revision: 1,
            receive: Some(ReceiveIntent {
                video: Some(VideoIntent {
                    tracks: ids
                        .iter()
                        .map(|id| VideoTrackIntent {
                            track_id: id.clone(),
                            ..Default::default()
                        })
                        .collect(),
                }),
                audio: Some(AudioIntent {
                    tracks: ids
                        .iter()
                        .map(|id| AudioTrackIntent {
                            track_id: id.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let mut state = IntentState::default();
        let IntentDecision::Accept(plan) = state.plan(&intent, &media(), 1).unwrap() else {
            panic!("fresh intent must be accepted");
        };
        let (_, receive) = state.commit(*plan);
        let receive = receive.unwrap();
        let video = receive.video.unwrap().tracks;
        let audio = receive.audio.unwrap().tracks;
        assert_eq!(video.len(), 1);
        assert_eq!(audio.len(), 1);
        assert_eq!(video[0].track_id, "é".repeat(64));
        assert_eq!(audio[0].track_id, "é".repeat(64));
    }

    #[test]
    fn catalog_snapshot_and_delta_remove_publisher_tracks_implicitly() {
        use pulsebeam_proto::signaling_v1::{catalog, server_message};
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let mut state = CatalogState::default();
        let plan = state
            .plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "mic")],
                recipient,
                "self",
            )
            .unwrap()
            .unwrap();
        let Some(server_message::Payload::Catalog(first)) = &plan.message.payload else {
            panic!("initial catalog message missing");
        };
        assert_eq!(first.revision, 1);
        assert!(matches!(first.state, Some(catalog::State::Snapshot(_))));
        state.commit(plan);
        let plan = state.plan([], [], recipient, "self").unwrap().unwrap();
        let Some(server_message::Payload::Catalog(next)) = &plan.message.payload else {
            panic!("catalog delta missing");
        };
        assert_eq!(next.revision, 2);
        let Some(catalog::State::Delta(delta)) = &next.state else {
            panic!("expected delta");
        };
        assert_eq!(delta.removed_participant_ids, vec![publisher.as_str()]);
        assert!(delta.removed_track_ids.is_empty());
        state.commit(plan);
        assert!(state.plan([], [], recipient, "self").unwrap().is_none());
    }

    #[test]
    fn native_session_orders_mapping_clear_before_catalog_removal() {
        use pulsebeam_proto::signaling_v1::server_message::Payload;
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let track = remote_audio(publisher, "mic");
        let mut rtc = str0m::Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        let mut session = NativeSession::default();
        session.set_cid(cid);
        session.apply_participants([peer(publisher, "alice")], []);
        let next =
            |session: &mut NativeSession, publications: &[crate::track::TrackMeta], audio| {
                let output = session
                    .poll(publications, (Vec::new(), audio), recipient, "self")
                    .unwrap()
                    .unwrap();
                let message = pulsebeam_proto::codec::decode_server(&output.bytes).unwrap();
                session.commit_sent();
                message.payload.unwrap()
            };
        assert!(matches!(
            next(&mut session, &[track.clone()], Vec::new()),
            Payload::Catalog(_)
        ));
        assert!(matches!(
            next(&mut session, &[track.clone()], vec![(2, track.id)]),
            Payload::Mapping(_)
        ));
        session.apply_participants([], [publisher]);
        assert!(matches!(
            next(&mut session, &[], vec![(2, track.id)]),
            Payload::Mapping(mapping) if mapping.audio.as_ref().unwrap().tracks.is_empty()
        ));
        assert!(matches!(
            next(&mut session, &[], vec![(2, track.id)]),
            Payload::Catalog(_)
        ));
        assert!(
            session
                .poll(&[], (Vec::new(), vec![(2, track.id)]), recipient, "self")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mapping_only_references_committed_catalog_with_unique_coordinates() {
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let id = remote_audio(publisher, "mic").id;
        let mut state = CatalogState::default();
        let plan = state
            .plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "mic")],
                recipient,
                "self",
            )
            .unwrap()
            .unwrap();
        assert!(matches!(
            state.mapping(7, [], [(1, id)]),
            Err(MappingError::UnknownTrack)
        ));
        state.commit(plan);
        let mapped = state.mapping(7, [], [(1, id)]).unwrap();
        assert_eq!(mapped.intent_revision, 7);
        assert!(mapped.video.unwrap().tracks.is_empty());
        assert_eq!(mapped.audio.unwrap().tracks[0].receiver_index, 1);
        assert!(matches!(
            state.mapping(7, [(1, id)], []),
            Err(MappingError::WrongKind)
        ));
        assert!(matches!(
            state.mapping(7, [], [(1, id), (2, id)]),
            Err(MappingError::DuplicateTrack)
        ));
        assert!(matches!(
            state.mapping(7, [], [(1, id), (1, id)]),
            Err(MappingError::DuplicateReceiver)
        ));
    }

    #[test]
    fn catalog_refuses_identity_reuse_after_removal() {
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let mut state = CatalogState::default();
        let first = state
            .plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "mic")],
                recipient,
                "self",
            )
            .unwrap()
            .unwrap();
        state.commit(first);
        let removed = state.plan([], [], recipient, "self").unwrap().unwrap();
        state.commit(removed);
        assert!(matches!(
            state.plan([peer(publisher, "bob")], [], recipient, "self"),
            Err(CatalogError::ChangedIdentity)
        ));
        assert!(matches!(
            state.plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "different")],
                recipient,
                "self",
            ),
            Ok(Some(_))
        ));
        let mut changed_track = remote_audio(publisher, "mic");
        changed_track.label = Some("renamed".to_owned());
        assert!(matches!(
            state.plan(
                [peer(publisher, "alice")],
                [changed_track],
                recipient,
                "self"
            ),
            Err(CatalogError::ChangedIdentity)
        ));
    }

    #[test]
    fn catalog_rejects_duplicate_external_identity_and_oversized_snapshots() {
        let recipient = crate::entity::ParticipantId::new();
        let a = crate::entity::ParticipantId::new();
        let b = crate::entity::ParticipantId::new();
        let state = CatalogState::default();
        assert!(matches!(
            state.plan([peer(a, "same"), peer(b, "same")], [], recipient, "self"),
            Err(CatalogError::DuplicateIdentity)
        ));
        let peers = (0..600)
            .map(|n| {
                peer(
                    crate::entity::ParticipantId::new(),
                    &format!("peer-{n:03}-abcdefghijklmnopqrst"),
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            state.plan(peers, [], recipient, "self"),
            Err(CatalogError::OversizedSnapshot)
        ));
    }

    #[test]
    fn first_usable_entries_win_and_omission_preserves_binding() {
        let mut state = SenderBindings::default();
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Video, "wrong-kind"),
                track(0, WireTrackKind::Audio, "mic"),
                track(0, WireTrackKind::Audio, "later"),
                track(3, WireTrackKind::Audio, "mic"),
                track(2, WireTrackKind::Video, "camera"),
            ],
        };
        let plan = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(state.commit(plan).len(), 2);
        assert!(state.commit(state.plan(None, &media()).unwrap()).is_empty());
        let restored = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(restored.tracks.len(), 2);
    }

    #[test]
    fn established_relabel_or_rebind_is_fatal_without_partial_commit() {
        let mut state = SenderBindings::default();
        let first = SendIntent {
            tracks: vec![track(0, WireTrackKind::Audio, "mic")],
        };
        let plan = state.plan(Some(&first), &media()).unwrap();
        state.commit(plan);
        let relabel = SendIntent {
            tracks: vec![
                track(2, WireTrackKind::Video, "camera"),
                track(0, WireTrackKind::Audio, "other"),
            ],
        };
        assert!(matches!(
            state.plan(Some(&relabel), &media()),
            Err(SendBindingError::RelabeledSender)
        ));
        let moved = SendIntent {
            tracks: vec![track(3, WireTrackKind::Audio, "mic")],
        };
        assert!(matches!(
            state.plan(Some(&moved), &media()),
            Err(SendBindingError::MovedLabel)
        ));
        assert_eq!(state.plan(Some(&first), &media()).unwrap().tracks.len(), 1);
    }

    #[test]
    fn invalid_labels_and_unknown_indices_are_skipped() {
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Audio, ""),
                track(0, WireTrackKind::Audio, &"é".repeat(33)),
                track(9, WireTrackKind::Audio, "missing"),
                track(0, WireTrackKind::Audio, "valid"),
            ],
        };
        assert_eq!(
            SenderBindings::default()
                .plan(Some(&intent), &media())
                .unwrap()
                .tracks
                .len(),
            1
        );
    }
}
