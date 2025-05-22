use std::{
    collections::{HashMap, VecDeque},
    fmt::Display,
    ops::Deref,
    sync::Arc,
};

use str0m::{
    Rtc,
    media::{MediaData, Rid},
};
use tokio::{
    sync::mpsc::{self, error::SendError},
    task::{JoinSet, LocalSet},
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    context,
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackIn,
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    router::RouterHandle,
    voice_ranker::VoiceRanker,
};

#[derive(Debug)]
pub enum RoomControlMessage {
    PublishTrack(Arc<TrackIn>),
    AddParticipant(Arc<ParticipantId>, Rtc),
}

pub enum RoomDataMessage {
    ForwardVideo(Arc<TrackId>, Arc<MediaData>),
    ForwardAudio(Arc<TrackId>, Arc<MediaData>),
}

pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, Arc<TrackIn>>,
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct RoomActor {
    ctx: context::Context,
    control_rx: mpsc::Receiver<RoomControlMessage>,
    data_rx: mpsc::Receiver<RoomDataMessage>,
    handle: RoomHandle,

    participant_tasks: JoinSet<Arc<ParticipantId>>,

    // Data plane
    voice_ranker: VoiceRanker,
    participants: Vec<ParticipantHandle>,
    video_forwarding_rules: HashMap<(Arc<TrackId>, Option<Rid>), VecDeque<ParticipantHandle>>,
}

impl Actor for RoomActor {
    type ID = Arc<RoomId>;

    fn kind(&self) -> &'static str {
        "room"
    }

    fn id(&self) -> Self::ID {
        self.handle.room_id.clone()
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        loop {
            tokio::select! {
                res = self.receiver.recv() => {
                    match res {
                        Some(msg) => self.handle_control_message(msg).await,
                        None => break,
                    }
                }

                Some(Ok(participant_id)) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id).await;
                }

                else => break,
            }
        }

        Ok(())
    }
}

impl RoomActor {
    async fn handle_control_message(&mut self, msg: RoomControlMessage) {
        match msg {
            RoomControlMessage::AddParticipant(participant_id, rtc) => {
                let mut tracks = Vec::new();
                for meta in self.participants.values() {
                    for (track_id, _) in &meta.tracks {
                        tracks.push(track_id.clone());
                    }
                }

                let (participant_handle, participant_actor) = ParticipantHandle::new(
                    self.ctx.clone(),
                    self.handle.clone(),
                    self.router.clone(),
                    participant_id.clone(),
                    rtc,
                    tracks,
                );
                let participant_id = participant_handle.participant_id.clone();
                self.participants.insert(
                    participant_handle.participant_id.clone(),
                    ParticipantMeta {
                        handle: participant_handle.clone(),
                        tracks: HashMap::new(),
                    },
                );
                self.participant_tasks.spawn(
                    async move {
                        actor::run(participant_actor).await;
                        participant_id
                    }
                    .in_current_span(),
                );
            }
            RoomControlMessage::PublishTrack(track_id) => {
                let Some(origin) = self.participants.get_mut(&track_id.origin_participant) else {
                    tracing::warn!("{} is missing from participants, ignoring track", track_id);
                    return;
                };

                origin.tracks.insert(track_id.clone(), track_id.clone());
                let new_tracks = Arc::new(vec![track_id]);
                for (_, participant) in &self.participants {
                    let _ = participant.handle.add_tracks(new_tracks.clone()).await;
                }
            }
        };
    }

    async fn handle_participant_left(&mut self, participant_id: Arc<ParticipantId>) {
        // TODO: notify participant leaving
        let Some(participant) = self.participants.remove(&participant_id) else {
            return;
        };

        let tracks: Vec<Arc<TrackId>> = participant
            .tracks
            .into_values()
            .map(|t| t.meta.id.clone())
            .collect();
        let tracks = Arc::new(tracks);
        for (_, participant) in &self.participants {
            let _ = participant.handle.remove_tracks(tracks.clone()).await;
        }
    }

    async fn handle_data_message(&mut self, msg: RoomDataMessage) {
        match msg {
            RoomDataMessage::ForwardVideo(track_id, media) => {
                let Some(participants) = self
                    .video_forwarding_rules
                    .get(&(track_id.clone(), media.rid))
                else {
                    return;
                };

                for participant in participants {
                    let _ = participant.forward_video(track_id.clone(), media.clone());
                }
            }
            RoomDataMessage::ForwardAudio(track_id, media) => {
                let audio_level_val =
                    match (media.ext_vals.audio_level, media.ext_vals.voice_activity) {
                        (Some(level), Some(true)) => level, // VAD is true, and we have an audio level
                        _ => {
                            // No VAD, or no audio level, or VAD is false.
                            tracing::trace!(
                                "Audio packet for track {:?} without VAD/level, not ranking.",
                                track_id
                            );
                            return; // Don't process or forward
                        }
                    };

                if !self
                    .voice_ranker
                    .process_packet(track_id.clone(), audio_level_val)
                {
                    return; // Not dominant
                }

                // If we reach here, the packet is dominant and should be forwarded.
                for participant in &self.participants {
                    if track_id.origin_participant == participant.participant_id {
                        continue;
                    }
                    let _ = participant.forward_audio(track_id.clone(), media.clone());
                }
            }
        };
    }
}

#[derive(Clone)]
pub struct RoomHandle {
    pub control_tx: mpsc::Sender<RoomControlMessage>,
    pub data_tx: mpsc::Sender<RoomDataMessage>,
    pub room_id: Arc<RoomId>,
}

impl RoomHandle {
    pub fn new(ctx: context::Context, room_id: Arc<RoomId>) -> (Self, RoomActor) {
        let (control_tx, control_rx) = mpsc::channel(8);
        let (data_tx, data_rx) = mpsc::channel(128);
        let handle = RoomHandle {
            control_tx,
            data_tx,
            room_id: room_id.clone(),
        };
        let actor = RoomActor {
            ctx,
            control_rx,
            data_rx,
            handle: handle.clone(),
            participants: Vec::new(),
            participant_tasks: JoinSet::new(),
            voice_ranker: VoiceRanker::default(),
            video_forwarding_rules: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> Result<(), SendError<RoomControlMessage>> {
        self.control_tx
            .send(RoomControlMessage::AddParticipant(participant_id, rtc))
            .await
    }

    pub async fn publish(&self, track: Arc<TrackIn>) -> Result<(), SendError<RoomControlMessage>> {
        self.control_tx
            .send(RoomControlMessage::PublishTrack(track))
            .await
    }

    pub async fn forward_video(
        &self,
        track_id: Arc<TrackId>,
        media: Arc<MediaData>,
    ) -> Result<(), SendError<RoomDataMessage>> {
        self.data_tx
            .send(RoomDataMessage::ForwardVideo(track_id, media))
            .await
    }

    pub async fn forward_audio(
        &self,
        track_id: Arc<TrackId>,
        media: Arc<MediaData>,
    ) -> Result<(), SendError<RoomDataMessage>> {
        self.data_tx
            .send(RoomDataMessage::ForwardAudio(track_id, media))
            .await
    }
}

impl Display for RoomHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
