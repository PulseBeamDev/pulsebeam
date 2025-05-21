use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use str0m::media::MediaData;
use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    entity::{ParticipantId, RoomId, TrackId},
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    room::RoomHandle,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum RouterDataMessage {
    ForwardVideo(Arc<TrackId>, Arc<MediaData>),
    ForwardAudio(Arc<TrackId>, Arc<MediaData>),
}

#[derive(Debug)]
pub enum RouterControlMessage {}

pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, TrackHandle>,
}

pub struct RouterActor {
    data_rx: mpsc::Receiver<RouterDataMessage>,
    control_rx: mpsc::Receiver<RouterControlMessage>,

    participants: Vec<ParticipantHandle>,
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
            RouterDataMessage::ForwardVideo(track_id, media) => {}
            RouterDataMessage::ForwardAudio(track_id, media) => {
                for participant in &self.participants {
                    participant.forward_media(track, data)
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
