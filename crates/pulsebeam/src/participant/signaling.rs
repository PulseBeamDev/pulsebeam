use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use str0m::channel::ChannelId;

use crate::log::LogCtx;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling_v1 as v1;

#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    #[error("Invalid v1 signaling message")]
    DecodeFailed,
    #[error("Signaling response queue is full")]
    ResponseBackpressured,
}

pub(crate) struct SignalingSnapshot {
    pub(crate) publications: Vec<crate::track::TrackMeta>,
    pub(crate) participants: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CatalogBuildError {
    #[error("catalog publication has no application label")]
    MissingLabel,
    #[error("catalog publication references a participant outside the room view")]
    UnknownParticipant,
    #[error("catalog contains duplicate application identity")]
    DuplicateIdentity,
    #[error("catalog identity exceeds a protocol bound")]
    ValueOutOfBounds,
    #[error("catalog snapshot exceeds the signaling codec limit")]
    SnapshotTooLarge,
    #[error("catalog track ID does not match its immutable identity")]
    IdentityMismatch,
}

fn valid_protocol_string(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes
}

pub(crate) fn build_catalog(
    recipient: crate::entity::ParticipantId,
    recipient_external_id: &str,
    snapshot: &SignalingSnapshot,
) -> Result<v1::CatalogSnapshot, CatalogBuildError> {
    if !valid_protocol_string(recipient_external_id, 256) {
        return Err(CatalogBuildError::ValueOutOfBounds);
    }
    let mut external_ids = HashSet::from_iter([recipient_external_id.to_owned()]);
    for (participant_id, external_id) in &snapshot.participants {
        if participant_id.as_str() == recipient.as_str() {
            continue;
        }
        if !valid_protocol_string(participant_id, 128) || !valid_protocol_string(external_id, 256) {
            return Err(CatalogBuildError::ValueOutOfBounds);
        }
        if !external_ids.insert(external_id.clone()) {
            return Err(CatalogBuildError::DuplicateIdentity);
        }
    }
    let mut participants: Vec<_> = snapshot
        .participants
        .iter()
        .filter(|(id, _)| id.as_str() != recipient.as_str())
        .map(
            |(participant_id, participant_external_id)| v1::Participant {
                participant_id: participant_id.clone(),
                participant_external_id: participant_external_id.clone(),
            },
        )
        .collect();
    participants.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));

    let mut track_ids = HashSet::new();
    let mut selectors = HashSet::new();
    let mut tracks = Vec::new();
    for meta in &snapshot.publications {
        if meta.origin == recipient || meta.id.kind() == crate::entity::TrackKind::Data {
            continue;
        }
        let participant_id = meta.origin.as_str();
        if !snapshot.participants.contains_key(&participant_id) {
            return Err(CatalogBuildError::UnknownParticipant);
        }
        let Some(label) = meta.label.clone() else {
            return Err(CatalogBuildError::MissingLabel);
        };
        if !valid_protocol_string(&meta.id.as_str(), 128) || !valid_protocol_string(&label, 64) {
            return Err(CatalogBuildError::ValueOutOfBounds);
        }
        let kind = match meta.id.kind() {
            crate::entity::TrackKind::Audio => v1::TrackKind::Audio,
            crate::entity::TrackKind::Video => v1::TrackKind::Video,
            crate::entity::TrackKind::Data => continue,
        };
        if meta.id != meta.origin.derive_track_id(meta.id.kind(), &label) {
            return Err(CatalogBuildError::IdentityMismatch);
        }
        if !track_ids.insert(meta.id)
            || !selectors.insert((meta.origin, kind as i32, label.clone()))
        {
            return Err(CatalogBuildError::DuplicateIdentity);
        }
        tracks.push(v1::RemoteTrack {
            track_id: meta.id.as_str(),
            participant_id,
            kind: kind.into(),
            label,
        });
    }
    tracks.sort_by(|left, right| left.track_id.cmp(&right.track_id));
    let catalog = v1::CatalogSnapshot {
        participants,
        tracks,
    };
    let message = v1::ServerMessage {
        payload: Some(v1::server_message::Payload::Catalog(v1::Catalog {
            revision: 1,
            state: Some(v1::catalog::State::Snapshot(catalog.clone())),
        })),
    };
    if message.encoded_len() > pulsebeam_proto::codec::MAX_MESSAGE_SIZE {
        return Err(CatalogBuildError::SnapshotTooLarge);
    }
    Ok(catalog)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum V1OutputBuildError {
    #[error(transparent)]
    Catalog(#[from] CatalogBuildError),
    #[error("catalog reuses a participant ID for a different external identity")]
    ParticipantIdentityChanged,
    #[error("catalog reuses a track ID for a different immutable identity")]
    TrackIdentityChanged,
    #[error("mapping references a track absent from the catalog")]
    MappingUnknownTrack,
    #[error("mapping references a track with the wrong media kind")]
    MappingWrongKind,
    #[error("mapping must include both video and audio groups")]
    IncompleteMapping,
    #[error("mapping repeats a receiver or track identity")]
    DuplicateMapping,
}

#[derive(Default)]
struct V1CatalogHistory {
    participants: HashMap<String, String>,
    tracks: HashMap<String, (String, i32, String)>,
}

impl V1CatalogHistory {
    fn validate(&self, catalog: &v1::CatalogSnapshot) -> Result<(), V1OutputBuildError> {
        for participant in &catalog.participants {
            if self
                .participants
                .get(&participant.participant_id)
                .is_some_and(|known| known != &participant.participant_external_id)
            {
                return Err(V1OutputBuildError::ParticipantIdentityChanged);
            }
        }
        for track in &catalog.tracks {
            let identity = (
                track.participant_id.clone(),
                track.kind,
                track.label.clone(),
            );
            if self
                .tracks
                .get(&track.track_id)
                .is_some_and(|known| known != &identity)
            {
                return Err(V1OutputBuildError::TrackIdentityChanged);
            }
        }
        Ok(())
    }

    fn commit(&mut self, catalog: &v1::CatalogSnapshot) {
        for participant in &catalog.participants {
            self.participants.insert(
                participant.participant_id.clone(),
                participant.participant_external_id.clone(),
            );
        }
        for track in &catalog.tracks {
            self.tracks.insert(
                track.track_id.clone(),
                (
                    track.participant_id.clone(),
                    track.kind,
                    track.label.clone(),
                ),
            );
        }
    }
}

fn validate_mapping(
    catalog: &v1::CatalogSnapshot,
    mapping: &v1::Mapping,
) -> Result<(), V1OutputBuildError> {
    let tracks: BTreeMap<_, _> = catalog
        .tracks
        .iter()
        .map(|track| (track.track_id.as_str(), track.kind))
        .collect();
    let mut receiver_indices = HashSet::new();
    for (mappings, expected_kind) in [
        (&mapping.video, v1::TrackKind::Video),
        (&mapping.audio, v1::TrackKind::Audio),
    ] {
        let Some(mappings) = mappings else {
            return Err(V1OutputBuildError::IncompleteMapping);
        };
        let mut track_ids = HashSet::new();
        for mapping in &mappings.tracks {
            let Some(kind) = tracks.get(mapping.track_id.as_str()) else {
                return Err(V1OutputBuildError::MappingUnknownTrack);
            };
            if *kind != expected_kind as i32 {
                return Err(V1OutputBuildError::MappingWrongKind);
            }
            if !receiver_indices.insert(mapping.receiver_index)
                || !track_ids.insert(mapping.track_id.as_str())
            {
                return Err(V1OutputBuildError::DuplicateMapping);
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct V1Intent {
    pub(crate) revision: u64,
    pub(crate) publications: Vec<crate::participant::intent::NativePublication>,
    pub(crate) video: Vec<V1VideoIntent>,
    pub(crate) audio: Vec<V1AudioIntent>,
    pub(crate) audio_auto: bool,
}
#[derive(Clone)]
pub(crate) struct V1VideoIntent {
    pub(crate) track_id: String,
    pub(crate) target_height: u32,
    pub(crate) min_height: u32,
    pub(crate) min_fps: u32,
    pub(crate) priority: u32,
    pub(crate) playout: crate::participant::downstream::PlayoutPolicy,
}
#[derive(Clone)]
pub(crate) struct V1AudioIntent {
    pub(crate) track_id: String,
    pub(crate) playout: crate::participant::downstream::PlayoutPolicy,
}
pub(crate) enum V1IntentResult {
    Mapping(v1::Mapping),
    ProtocolError(v1::Error),
    Reconnect,
}

fn playout(delay: Option<v1::PlayoutDelay>) -> crate::participant::downstream::PlayoutPolicy {
    delay
        .map(|delay| {
            crate::participant::downstream::PlayoutPolicy::fixed((delay.min_ms, delay.max_ms))
        })
        .unwrap_or(crate::participant::downstream::PlayoutPolicy::Default)
}

pub(crate) fn normalize_v1_intent(intent: v1::Intent) -> V1Intent {
    let publications = intent
        .send
        .unwrap_or_default()
        .tracks
        .into_iter()
        .map(|track| crate::participant::intent::NativePublication {
            sender_index: Some(track.sender_index),
            kind: match v1::TrackKind::try_from(track.kind) {
                Ok(v1::TrackKind::Audio) => Some(crate::entity::TrackKind::Audio),
                Ok(v1::TrackKind::Video) => Some(crate::entity::TrackKind::Video),
                _ => None,
            },
            label: track.label,
        })
        .collect();
    let receive = intent.receive.unwrap_or_default();
    let mut video_ids = HashSet::new();
    let video = receive
        .video
        .unwrap_or_default()
        .tracks
        .into_iter()
        .filter_map(|track| {
            video_ids.insert(track.track_id.clone()).then(|| {
                let options = track.options.unwrap_or_default();
                V1VideoIntent {
                    track_id: track.track_id,
                    target_height: options.height,
                    min_height: options.min_height,
                    min_fps: options.min_fps,
                    priority: options.priority,
                    playout: playout(options.playout_delay),
                }
            })
        })
        .collect();
    let audio = receive.audio.unwrap_or_default();
    let audio_auto = !matches!(
        v1::AudioMode::try_from(audio.mode),
        Ok(v1::AudioMode::ExplicitOnly)
    );
    let mut audio_ids = HashSet::new();
    let audio = audio
        .tracks
        .into_iter()
        .filter_map(|track| {
            audio_ids
                .insert(track.track_id.clone())
                .then(|| V1AudioIntent {
                    track_id: track.track_id,
                    playout: playout(track.options.and_then(|options| options.playout_delay)),
                })
        })
        .collect();
    V1Intent {
        revision: intent.revision,
        publications,
        video,
        audio,
        audio_auto,
    }
}

pub(crate) struct SignalingOutput {
    pub(crate) cid: ChannelId,
    pub(crate) bytes: Vec<u8>,
}
struct Desired {
    catalog: v1::CatalogSnapshot,
    mapping: v1::Mapping,
}
struct Pending {
    bytes: Vec<u8>,
    commit: Commit,
}
enum Commit {
    Catalog {
        catalog: v1::CatalogSnapshot,
        revision: u64,
    },
    Mapping {
        mapping: v1::Mapping,
        stale_ack: bool,
    },
}

#[derive(Default)]
struct Scheduler {
    desired: Option<Desired>,
    delivered_catalog: Option<v1::CatalogSnapshot>,
    delivered_mapping: Option<v1::Mapping>,
    catalog_revision: u64,
    pending: Option<Pending>,
    resnapshot: bool,
    stale_ack_count: usize,
}
impl Scheduler {
    fn stage(&mut self, desired: Desired) {
        let desired_changed = self.desired.as_ref().is_some_and(|current| {
            current.catalog != desired.catalog || current.mapping != desired.mapping
        });
        if desired_changed
            && self.pending.as_ref().is_some_and(|pending| {
                matches!(
                    pending.commit,
                    Commit::Mapping {
                        stale_ack: true,
                        ..
                    }
                )
            })
        {
            self.pending = None;
        }
        self.desired = Some(desired);
    }
    fn request_resnapshot(&mut self) {
        self.resnapshot = true;
    }
    fn request_stale_ack(&mut self) {
        self.stale_ack_count = self.stale_ack_count.saturating_add(1);
    }
    fn poll(&mut self) -> Option<Vec<u8>> {
        if let Some(pending) = &self.pending {
            return Some(pending.bytes.clone());
        }
        let desired = self.desired.as_ref()?;
        let (message, commit) = match &self.delivered_catalog {
            None => {
                let revision = self.catalog_revision.checked_add(1)?;
                (
                    catalog_snapshot(revision, desired.catalog.clone()),
                    Commit::Catalog {
                        catalog: desired.catalog.clone(),
                        revision,
                    },
                )
            }
            Some(delivered) if delivered != &desired.catalog => {
                let cleared = self
                    .delivered_mapping
                    .as_ref()
                    .map(|mapping| clear_absent(&desired.catalog, mapping));
                if let Some(mapping) =
                    cleared.filter(|mapping| self.delivered_mapping.as_ref() != Some(mapping))
                {
                    (
                        mapping_message(mapping.clone()),
                        Commit::Mapping {
                            mapping,
                            stale_ack: false,
                        },
                    )
                } else {
                    let revision = self.catalog_revision.checked_add(1)?;
                    (
                        if self.resnapshot {
                            catalog_snapshot(revision, desired.catalog.clone())
                        } else {
                            catalog_delta(revision, delivered, &desired.catalog)
                        },
                        Commit::Catalog {
                            catalog: desired.catalog.clone(),
                            revision,
                        },
                    )
                }
            }
            Some(_) if self.resnapshot => {
                let revision = self.catalog_revision.checked_add(1)?;
                (
                    catalog_snapshot(revision, desired.catalog.clone()),
                    Commit::Catalog {
                        catalog: desired.catalog.clone(),
                        revision,
                    },
                )
            }
            Some(_) if self.delivered_mapping.as_ref() != Some(&desired.mapping) => (
                mapping_message(desired.mapping.clone()),
                Commit::Mapping {
                    mapping: desired.mapping.clone(),
                    stale_ack: self.stale_ack_count != 0,
                },
            ),
            Some(_) if self.stale_ack_count != 0 => (
                mapping_message(desired.mapping.clone()),
                Commit::Mapping {
                    mapping: desired.mapping.clone(),
                    stale_ack: true,
                },
            ),
            Some(_) => return None,
        };
        let bytes = match pulsebeam_proto::codec::encode_server(&message) {
            Ok(bytes) => bytes,
            Err(pulsebeam_proto::codec::EncodeError::UncompressedTooLarge)
                if matches!(
                    &message.payload,
                    Some(v1::server_message::Payload::Catalog(v1::Catalog {
                        state: Some(v1::catalog::State::Delta(_)),
                        ..
                    }))
                ) =>
            {
                let Commit::Catalog { catalog, revision } = &commit else {
                    return None;
                };
                pulsebeam_proto::codec::encode_server(&catalog_snapshot(*revision, catalog.clone()))
                    .ok()?
            }
            Err(_) => return None,
        };
        self.pending = Some(Pending {
            bytes: bytes.clone(),
            commit,
        });
        Some(bytes)
    }
    fn commit_sent(&mut self) -> Option<v1::CatalogSnapshot> {
        match self.pending.take()?.commit {
            Commit::Catalog { catalog, revision } => {
                self.delivered_catalog = Some(catalog.clone());
                self.catalog_revision = revision;
                self.resnapshot = false;
                Some(catalog)
            }
            Commit::Mapping { mapping, stale_ack } => {
                self.delivered_mapping = Some(mapping);
                if stale_ack {
                    self.stale_ack_count = self.stale_ack_count.saturating_sub(1);
                }
                None
            }
        }
    }
}
fn catalog_snapshot(revision: u64, catalog: v1::CatalogSnapshot) -> v1::ServerMessage {
    v1::ServerMessage {
        payload: Some(v1::server_message::Payload::Catalog(v1::Catalog {
            revision,
            state: Some(v1::catalog::State::Snapshot(catalog)),
        })),
    }
}
fn mapping_message(mapping: v1::Mapping) -> v1::ServerMessage {
    v1::ServerMessage {
        payload: Some(v1::server_message::Payload::Mapping(mapping)),
    }
}
fn clear_absent(catalog: &v1::CatalogSnapshot, mapping: &v1::Mapping) -> v1::Mapping {
    let known: BTreeSet<_> = catalog.tracks.iter().map(|track| &track.track_id).collect();
    let retain = |tracks: &Option<v1::TrackMappings>| v1::TrackMappings {
        tracks: tracks
            .as_ref()
            .into_iter()
            .flat_map(|tracks| &tracks.tracks)
            .filter(|track| known.contains(&track.track_id))
            .cloned()
            .collect(),
    };
    v1::Mapping {
        intent_revision: mapping.intent_revision,
        video: Some(retain(&mapping.video)),
        audio: Some(retain(&mapping.audio)),
    }
}
fn catalog_delta(
    revision: u64,
    old: &v1::CatalogSnapshot,
    new: &v1::CatalogSnapshot,
) -> v1::ServerMessage {
    let old_participants: BTreeMap<_, _> = old
        .participants
        .iter()
        .map(|value| (&value.participant_id, value))
        .collect();
    let new_participants: BTreeMap<_, _> = new
        .participants
        .iter()
        .map(|value| (&value.participant_id, value))
        .collect();
    let old_tracks: BTreeMap<_, _> = old
        .tracks
        .iter()
        .map(|value| (&value.track_id, value))
        .collect();
    let new_tracks: BTreeMap<_, _> = new
        .tracks
        .iter()
        .map(|value| (&value.track_id, value))
        .collect();
    let removed_participant_ids: Vec<_> = old_participants
        .keys()
        .filter(|id| !new_participants.contains_key(*id))
        .map(|id| (*id).clone())
        .collect();
    let removed_participants: BTreeSet<_> = removed_participant_ids.iter().cloned().collect();
    v1::ServerMessage {
        payload: Some(v1::server_message::Payload::Catalog(v1::Catalog {
            revision,
            state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                added_participants: new_participants
                    .iter()
                    .filter(|(id, _)| !old_participants.contains_key(*id))
                    .map(|(_, value)| (*value).clone())
                    .collect(),
                removed_participant_ids,
                added_tracks: new_tracks
                    .iter()
                    .filter(|(id, _)| !old_tracks.contains_key(*id))
                    .map(|(_, value)| (*value).clone())
                    .collect(),
                removed_track_ids: old_tracks
                    .iter()
                    .filter(|(id, track)| {
                        !new_tracks.contains_key(*id)
                            && !removed_participants.contains(&track.participant_id)
                    })
                    .map(|(id, _)| (*id).clone())
                    .collect(),
            })),
        })),
    }
}

enum AuthorizationResponse {
    Accepted(i64),
    Rejected,
}
pub(crate) const MAX_PENDING_AUTHORIZATION_RESPONSES: usize = 64;
struct Output {
    recipient_external_id: String,
    history: V1CatalogHistory,
    scheduler: Scheduler,
    authorization_responses: VecDeque<AuthorizationResponse>,
    pending_authorization: Option<Vec<u8>>,
    terminal: Option<Vec<u8>>,
}

pub struct Signaling {
    ctx: LogCtx,
    pub cid: Option<ChannelId>,
    dirty: bool,
    participants: HashMap<String, String>,
    output: Option<Output>,
    v1_revision: u64,
    v1_intent: Option<V1Intent>,
}
impl Signaling {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            cid: None,
            dirty: false,
            participants: HashMap::new(),
            output: None,
            v1_revision: 0,
            v1_intent: None,
        }
    }
    pub(crate) fn new_v1(ctx: LogCtx, recipient_external_id: String) -> Self {
        Self {
            ctx,
            cid: None,
            dirty: true,
            participants: HashMap::new(),
            output: Some(Output {
                recipient_external_id,
                history: V1CatalogHistory::default(),
                scheduler: Scheduler::default(),
                authorization_responses: VecDeque::new(),
                pending_authorization: None,
                terminal: None,
            }),
            v1_revision: 0,
            v1_intent: None,
        }
    }
    pub(crate) fn is_v1(&self) -> bool {
        self.output.is_some()
    }
    pub(crate) fn is_terminal(&self) -> bool {
        self.output
            .as_ref()
            .is_some_and(|output| output.terminal.is_some())
    }
    pub fn set_cid(&mut self, cid: ChannelId) {
        if self.cid.replace(cid).is_some()
            && let Some(output) = &mut self.output
        {
            output.scheduler.request_resnapshot();
        }
        self.dirty = true;
    }
    pub fn clear_cid(&mut self, cid: ChannelId) -> bool {
        if self.cid != Some(cid) {
            return false;
        }
        self.cid = None;
        true
    }
    pub(crate) fn v1_is_fresh(&self, revision: u64) -> bool {
        revision != 0 && revision > self.v1_revision
    }
    pub(crate) fn accept_v1_intent(&mut self, intent: V1Intent) {
        self.v1_revision = intent.revision;
        self.v1_intent = Some(intent);
    }
    pub(crate) fn v1_revision(&self) -> u64 {
        self.v1_revision
    }
    pub(crate) fn v1_intent(&self) -> Option<&V1Intent> {
        self.v1_intent.as_ref()
    }
    pub fn mark_tracks_dirty(&mut self) {
        self.dirty = true;
    }
    pub fn mark_assignments_dirty(&mut self) {
        self.dirty = true;
    }
    pub(crate) fn participants_snapshot(&self) -> HashMap<String, String> {
        self.participants.clone()
    }
    pub fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = crate::participant::RoomParticipant>,
        removed: impl IntoIterator<Item = crate::entity::ParticipantId>,
    ) {
        for participant in added {
            self.participants.insert(
                participant.id.as_str(),
                participant.external_id.as_str().to_owned(),
            );
        }
        for participant in removed {
            self.participants.remove(&participant.as_str());
        }
        self.dirty = true;
    }
    pub(crate) fn stage_v1_output(
        &mut self,
        snapshot: &SignalingSnapshot,
        mapping: v1::Mapping,
    ) -> Result<(), V1OutputBuildError> {
        let Some(output) = &mut self.output else {
            return Ok(());
        };
        let catalog = build_catalog(
            self.ctx.participant_id,
            &output.recipient_external_id,
            snapshot,
        )?;
        output.history.validate(&catalog)?;
        validate_mapping(&catalog, &mapping)?;
        output.scheduler.stage(Desired { catalog, mapping });
        Ok(())
    }
    pub(crate) fn request_stale_v1_ack(&mut self) {
        if let Some(output) = &mut self.output {
            output.scheduler.request_stale_ack();
            self.dirty = true;
        }
    }
    pub(crate) fn stage_authorization(&mut self, expiry: i64) -> bool {
        let Some(output) = &mut self.output else {
            return false;
        };
        if output.authorization_responses.len() >= MAX_PENDING_AUTHORIZATION_RESPONSES {
            return false;
        }
        output
            .authorization_responses
            .push_back(AuthorizationResponse::Accepted(expiry));
        true
    }
    pub(crate) fn stage_authorization_rejected(&mut self) -> bool {
        let Some(output) = &mut self.output else {
            return false;
        };
        if output.authorization_responses.len() >= MAX_PENDING_AUTHORIZATION_RESPONSES {
            return false;
        }
        output
            .authorization_responses
            .push_back(AuthorizationResponse::Rejected);
        true
    }
    pub(crate) fn stage_authorization_expired(&mut self) -> bool {
        self.cid.is_some() && self.output.is_some() && {
            self.stage_v1_terminal_error(v1::Error {
                code: v1::ErrorCode::AuthorizationExpired.into(),
                message: "authorization expired".to_owned(),
                fatal: true,
                intent_revision: None,
            });
            true
        }
    }
    pub(crate) fn stage_v1_invalid_message(&mut self) {
        self.stage_v1_terminal_error(v1::Error {
            code: v1::ErrorCode::InvalidMessage.into(),
            message: "invalid signaling message".to_owned(),
            fatal: true,
            intent_revision: None,
        });
    }
    pub(crate) fn stage_v1_internal_error(&mut self) {
        self.stage_v1_terminal_error(v1::Error {
            code: v1::ErrorCode::Internal.into(),
            message: "internal signaling invariant failed".to_owned(),
            fatal: true,
            intent_revision: None,
        });
    }
    pub(crate) fn stage_v1_terminal_error(&mut self, error: v1::Error) {
        self.stage_terminal(v1::ServerMessage {
            payload: Some(v1::server_message::Payload::Error(error)),
        });
    }
    pub(crate) fn stage_v1_reconnect(&mut self) {
        self.stage_terminal(v1::ServerMessage {
            payload: Some(v1::server_message::Payload::Reconnect(v1::Reconnect {})),
        });
    }
    fn stage_terminal(&mut self, message: v1::ServerMessage) {
        let Some(output) = &mut self.output else {
            return;
        };
        if output.terminal.is_some() {
            return;
        }
        let Ok(bytes) = pulsebeam_proto::codec::encode_server(&message) else {
            return;
        };
        output.scheduler = Scheduler::default();
        output.authorization_responses.clear();
        output.pending_authorization = None;
        output.terminal = Some(bytes);
        self.dirty = false;
    }
    pub(crate) fn needs_poll(&self) -> bool {
        self.cid.is_some()
            && self.output.as_ref().is_some_and(|output| {
                output.terminal.is_some()
                    || output.pending_authorization.is_some()
                    || !output.authorization_responses.is_empty()
                    || self.dirty
            })
    }
    pub(crate) fn poll(&mut self, _snapshot: &SignalingSnapshot) -> Option<SignalingOutput> {
        let cid = self.cid?;
        let output = self.output.as_mut()?;
        if let Some(bytes) = &output.terminal {
            return Some(SignalingOutput {
                cid,
                bytes: bytes.clone(),
            });
        }
        if let Some(bytes) = &output.pending_authorization {
            return Some(SignalingOutput {
                cid,
                bytes: bytes.clone(),
            });
        }
        if output.scheduler.pending.is_some() {
            let bytes = output.scheduler.poll()?;
            return Some(SignalingOutput { cid, bytes });
        }
        if let Some(response) = output.authorization_responses.pop_front() {
            let payload = match response {
                AuthorizationResponse::Accepted(expires_at_unix_seconds) => {
                    v1::server_message::Payload::Authorization(v1::Authorization {
                        expires_at_unix_seconds,
                    })
                }
                AuthorizationResponse::Rejected => v1::server_message::Payload::Error(v1::Error {
                    code: v1::ErrorCode::AuthorizationRejected.into(),
                    message: "authorization renewal rejected".to_owned(),
                    fatal: false,
                    intent_revision: None,
                }),
            };
            let bytes = pulsebeam_proto::codec::encode_server(&v1::ServerMessage {
                payload: Some(payload),
            })
            .ok()?;
            output.pending_authorization = Some(bytes.clone());
            return Some(SignalingOutput { cid, bytes });
        }
        let bytes = output.scheduler.poll()?;
        Some(SignalingOutput { cid, bytes })
    }
    pub(crate) fn commit_sent(&mut self) -> bool {
        let Some(output) = &mut self.output else {
            return false;
        };
        if output.terminal.take().is_some() {
            return true;
        }
        if output.pending_authorization.take().is_some() {
            return false;
        }
        if let Some(catalog) = output.scheduler.commit_sent() {
            output.history.commit(&catalog);
        }
        self.dirty = true;
        false
    }
    pub(crate) fn retry_pending(&mut self) {}
    #[cfg(test)]
    pub(crate) fn v1_test_catalog_revision(&self) -> u64 {
        self.output
            .as_ref()
            .map_or(0, |output| output.scheduler.catalog_revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(revision: u64) -> v1::Mapping {
        v1::Mapping {
            intent_revision: revision,
            video: Some(v1::TrackMappings { tracks: Vec::new() }),
            audio: Some(v1::TrackMappings { tracks: Vec::new() }),
        }
    }

    fn v1_signaling() -> Signaling {
        let room = crate::entity::RoomExternalId::new("signaling-test").unwrap();
        let participant_id = crate::entity::ParticipantId::new();
        let mut rtc = str0m::Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        let mut signaling = Signaling::new_v1(
            LogCtx {
                room_id: crate::entity::RoomId::from_external(&room),
                participant_id,
            },
            "recipient".to_owned(),
        );
        signaling.set_cid(cid);
        let snapshot = SignalingSnapshot {
            publications: Vec::new(),
            participants: HashMap::default(),
        };
        signaling.stage_v1_output(&snapshot, mapping(1)).unwrap();
        signaling
    }

    #[test]
    fn snapshot_bound_checks_complete_catalog_not_only_deltas() {
        let recipient = crate::entity::ParticipantId::new();
        let snapshot = |count: usize| SignalingSnapshot {
            publications: Vec::new(),
            participants: (0..count)
                .map(|index| {
                    (
                        format!("p{index:03}"),
                        format!("e{index:03}{}", "x".repeat(239)),
                    )
                })
                .collect(),
        };
        assert!(build_catalog(recipient, "recipient", &snapshot(126)).is_ok());
        assert_eq!(
            build_catalog(recipient, "recipient", &snapshot(130)),
            Err(CatalogBuildError::SnapshotTooLarge)
        );
    }

    #[test]
    fn v1_scheduler_emits_catalog_before_mapping() {
        let catalog = v1::CatalogSnapshot {
            participants: Vec::new(),
            tracks: Vec::new(),
        };
        let expected_mapping = mapping(1);
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog,
            mapping: expected_mapping.clone(),
        });

        let catalog = scheduler.poll().expect("initial catalog");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&catalog),
            Ok(v1::ServerMessage {
                payload: Some(v1::server_message::Payload::Catalog(_))
            })
        ));
        scheduler.commit_sent();
        let mapping = scheduler.poll().expect("mapping after catalog commit");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&mapping),
            Ok(v1::ServerMessage {
                payload: Some(v1::server_message::Payload::Mapping(value))
            }) if value == expected_mapping
        ));
    }

    #[test]
    fn oversized_delta_falls_back_to_encodable_snapshot_without_losing_revision() {
        let catalog = |prefix: &str| v1::CatalogSnapshot {
            participants: (0..126)
                .map(|index| v1::Participant {
                    participant_id: format!("{prefix}{index:03}"),
                    participant_external_id: format!("{prefix}{index:03}{}", "x".repeat(239)),
                })
                .collect(),
            tracks: Vec::new(),
        };
        let first = catalog("a");
        let second = catalog("b");
        assert!(
            catalog_snapshot(2, second.clone()).encoded_len()
                <= pulsebeam_proto::codec::MAX_MESSAGE_SIZE
        );
        assert!(
            catalog_delta(2, &first, &second).encoded_len()
                > pulsebeam_proto::codec::MAX_MESSAGE_SIZE
        );
        assert!(pulsebeam_proto::codec::encode_server(&catalog_snapshot(1, first.clone())).is_ok());
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog: first,
            mapping: mapping(1),
        });
        scheduler.poll().unwrap();
        scheduler.commit_sent();
        scheduler.poll().unwrap();
        scheduler.commit_sent();
        scheduler.stage(Desired {
            catalog: second.clone(),
            mapping: mapping(1),
        });
        let wire = scheduler.poll().expect("fallback snapshot");
        assert_eq!(scheduler.poll(), Some(wire.clone()));
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&wire),
            Ok(v1::ServerMessage {
                payload: Some(v1::server_message::Payload::Catalog(v1::Catalog {
                    revision: 2,
                    state: Some(v1::catalog::State::Snapshot(snapshot)),
                })),
            }) if snapshot == second
        ));
        scheduler.commit_sent();
        assert_eq!(scheduler.catalog_revision, 2);
        assert!(scheduler.poll().is_none());
    }

    #[test]
    fn failed_catalog_retries_before_a_later_authorization() {
        let mut signaling = v1_signaling();
        let failed = signaling
            .poll(&SignalingSnapshot {
                publications: Vec::new(),
                participants: HashMap::default(),
            })
            .unwrap()
            .bytes;
        signaling.retry_pending();
        assert!(signaling.stage_authorization(42));

        let retry = signaling
            .poll(&SignalingSnapshot {
                publications: Vec::new(),
                participants: HashMap::default(),
            })
            .unwrap()
            .bytes;
        assert_eq!(retry, failed);
    }

    #[test]
    fn terminal_staging_keeps_its_first_payload() {
        let mut signaling = v1_signaling();
        signaling.stage_v1_invalid_message();
        signaling.stage_v1_reconnect();

        let bytes = signaling
            .poll(&SignalingSnapshot {
                publications: Vec::new(),
                participants: HashMap::default(),
            })
            .unwrap()
            .bytes;
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&bytes),
            Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Error(v1::Error { code, .. })) })
                if code == v1::ErrorCode::InvalidMessage as i32
        ));
    }

    #[test]
    fn forced_mapping_is_emitted_after_an_identical_delivered_mapping() {
        let expected = mapping(7);
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog: v1::CatalogSnapshot {
                participants: Vec::new(),
                tracks: Vec::new(),
            },
            mapping: expected.clone(),
        });
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.request_stale_ack();

        let bytes = scheduler.poll().expect("forced mapping");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&bytes),
            Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Mapping(mapping)) })
                if mapping == expected
        ));
    }

    #[test]
    fn two_stale_acknowledgements_each_emit_a_mapping() {
        let expected = mapping(9);
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog: v1::CatalogSnapshot {
                participants: Vec::new(),
                tracks: Vec::new(),
            },
            mapping: expected.clone(),
        });
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.request_stale_ack();
        scheduler.request_stale_ack();

        for _ in 0..2 {
            let bytes = scheduler.poll().expect("stale acknowledgement");
            assert!(matches!(
                pulsebeam_proto::codec::decode_server(&bytes),
                Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Mapping(mapping)) })
                    if mapping == expected
            ));
            scheduler.commit_sent();
        }
        assert!(scheduler.poll().is_none());
    }

    #[test]
    fn ordinary_mapping_retries_exact_bytes_after_desired_state_changes() {
        let catalog = v1::CatalogSnapshot {
            participants: Vec::new(),
            tracks: Vec::new(),
        };
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog: catalog.clone(),
            mapping: mapping(1),
        });
        scheduler.poll();
        scheduler.commit_sent();
        let rejected = scheduler.poll().expect("ordinary mapping");

        scheduler.stage(Desired {
            catalog,
            mapping: mapping(2),
        });

        assert_eq!(scheduler.poll().as_deref(), Some(rejected.as_slice()));
    }

    #[test]
    fn stale_acknowledgement_waits_for_removal_causality_and_uses_current_mapping() {
        let track = v1::RemoteTrack {
            track_id: "track".to_owned(),
            participant_id: "participant".to_owned(),
            kind: v1::TrackKind::Video.into(),
            label: "camera".to_owned(),
        };
        let mapped = v1::Mapping {
            intent_revision: 1,
            video: Some(v1::TrackMappings {
                tracks: vec![v1::TrackMapping {
                    receiver_index: 0,
                    track_id: track.track_id.clone(),
                }],
            }),
            audio: Some(v1::TrackMappings { tracks: Vec::new() }),
        };
        let empty = mapping(1);
        let mut scheduler = Scheduler::default();
        scheduler.stage(Desired {
            catalog: v1::CatalogSnapshot {
                participants: Vec::new(),
                tracks: vec![track],
            },
            mapping: mapped.clone(),
        });
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.poll();
        scheduler.commit_sent();
        scheduler.request_stale_ack();

        let rejected = scheduler.poll().expect("rejected stale acknowledgement");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&rejected),
            Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Mapping(mapping)) })
                if mapping == mapped
        ));
        scheduler.stage(Desired {
            catalog: v1::CatalogSnapshot {
                participants: Vec::new(),
                tracks: Vec::new(),
            },
            mapping: empty.clone(),
        });

        let clear = scheduler.poll().expect("clear removed mapping");
        assert_ne!(clear, rejected);
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&clear),
            Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Mapping(mapping)) })
                if mapping == empty
        ));
        scheduler.commit_sent();
        let catalog = scheduler.poll().expect("catalog removal");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&catalog),
            Ok(v1::ServerMessage {
                payload: Some(v1::server_message::Payload::Catalog(_))
            })
        ));
        scheduler.commit_sent();
        let stale = scheduler.poll().expect("current stale acknowledgement");
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&stale),
            Ok(v1::ServerMessage { payload: Some(v1::server_message::Payload::Mapping(mapping)) })
                if mapping == empty
        ));
        scheduler.commit_sent();
        assert!(scheduler.poll().is_none());
    }
}
