use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use str0m::media::{MediaData, Rid};
use tokio::sync::mpsc::{self, error::SendError};

use crate::{
    actor::{Actor, ActorError},
    entity::TrackId,
    participant::ParticipantHandle,
    room::RoomHandle,
    voice_ranker::VoiceRanker,
};

#[derive(Debug)]
pub enum RouterDataMessage {
    ForwardVideo(Arc<TrackId>, Arc<MediaData>),
    ForwardAudio(Arc<TrackId>, Arc<MediaData>),
}

#[derive(Debug)]
pub enum RouterControlMessage {}

pub struct RouterActor {
    data_rx: mpsc::Receiver<RouterDataMessage>,
    control_rx: mpsc::Receiver<RouterControlMessage>,

    voice_ranker: VoiceRanker,
    participants: Vec<ParticipantHandle>,
    video_forwarding_rules: HashMap<(Arc<TrackId>, Option<Rid>), VecDeque<ParticipantHandle>>,
}

impl Actor for RouterActor {
    type ID = &'static str;

    fn kind(&self) -> &'static str {
        "router"
    }

    fn id(&self) -> Self::ID {
        "0"
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        loop {
            tokio::select! {
                res = self.data_rx.recv() => {
                    match res {
                        Some(msg) => self.handle_message(msg).await,
                        None => break,
                    }
                }
                else => break,
            }
        }

        Ok(())
    }
}

impl RouterActor {
    async fn handle_message(&mut self, msg: RouterDataMessage) {
        match msg {
            RouterDataMessage::ForwardVideo(track_id, media) => {
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
            RouterDataMessage::ForwardAudio(track_id, media) => {
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
pub struct RouterHandle {
    pub data_tx: mpsc::Sender<RouterDataMessage>,
    pub control_tx: mpsc::Sender<RouterControlMessage>,
}

impl RouterHandle {
    pub fn new(room: RoomHandle) -> (Self, RouterActor) {
        let (data_tx, data_rx) = mpsc::channel(128);
        let (control_tx, control_rx) = mpsc::channel(8);
        let handle = RouterHandle {
            data_tx,
            control_tx,
        };
        let actor = RouterActor {
            data_rx,
            control_rx,
            participants: Vec::new(),
            voice_ranker: VoiceRanker::default(),
            video_forwarding_rules: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn forward_video(
        &self,
        track_id: Arc<TrackId>,
        media: Arc<MediaData>,
    ) -> Result<(), SendError<RouterDataMessage>> {
        self.data_tx
            .send(RouterDataMessage::ForwardVideo(track_id, media))
            .await
    }

    pub async fn forward_audio(
        &self,
        track_id: Arc<TrackId>,
        media: Arc<MediaData>,
    ) -> Result<(), SendError<RouterDataMessage>> {
        self.data_tx
            .send(RouterDataMessage::ForwardAudio(track_id, media))
            .await
    }
}
