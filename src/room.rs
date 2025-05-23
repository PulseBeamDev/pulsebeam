use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    ops::Deref,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt, stream::FuturesUnordered};
use str0m::{
    Rtc,
    media::{MediaData, Rid},
};
use tokio::{
    sync::mpsc::{
        self,
        error::{SendError, SendTimeoutError, TrySendError},
    },
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    context,
    entity::{ExternalParticipantId, ParticipantId, RoomId, TrackId},
    message::TrackIn,
    participant::ParticipantHandle,
    voice_ranker::VoiceRanker,
};

#[derive(Debug)]
pub enum RoomControlMessage {
    AddParticipant(Arc<ParticipantId>, Rtc),
    Subscribe(Arc<TrackId>),
    TrackAdded(Arc<TrackIn>),
    TrackRemoved(Arc<TrackId>),
}

#[derive(Debug, Clone)]
pub enum RoomEvent {
    TrackAdded(Arc<TrackIn>),
    TrackRemoved(Arc<TrackId>),
}

#[derive(Debug)]
pub enum RoomDataMessage {
    ForwardVideo(Arc<TrackId>, Arc<MediaData>),
    ForwardAudio(Arc<TrackId>, Arc<MediaData>),
}

pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, Arc<TrackIn>>,
}

type BroadcastEventFuture = Pin<Box<dyn Future<Output = Option<Arc<ParticipantId>>> + Send>>;

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct RoomActor {
    // Actor Loop
    ctx: context::Context,
    control_rx: mpsc::Receiver<RoomControlMessage>,
    data_rx: mpsc::Receiver<RoomDataMessage>,
    handle: RoomHandle,

    // Control Plane
    external_participants: HashSet<Arc<ExternalParticipantId>>,
    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    participant_tasks: JoinSet<Arc<ParticipantId>>,
    event_queue: FuturesUnordered<BroadcastEventFuture>,

    // Data Plane
    voice_ranker: VoiceRanker,
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
                Some(msg) = self.control_rx.recv() => {
                    self.handle_control_message(msg);
                }

                Some(msg) = self.data_rx.recv() => {
                    self.handle_data_message(msg);
                }

                res = self.participant_tasks.join_next() => {
                    if let Some(Ok(participant_id)) = res {
                        self.handle_participant_left(participant_id);
                    }
                }

                Some(res) = self.event_queue.next() => {
                    if let Some(participant_id) = res {
                        // TODO: participant_id must be closed forcefully
                    }
                }

                else => break,
            }
        }

        Ok(())
    }
}

impl RoomActor {
    fn handle_control_message(&mut self, msg: RoomControlMessage) {
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

                // TODO: kill older participant based on external id if exists
            }
            RoomControlMessage::TrackAdded(track) => {
                todo!();
                // let Some(origin) = self.participants.get_mut(&track_id.origin_participant) else {
                //     tracing::warn!("{} is missing from participants, ignoring track", track_id);
                //     return;
                // };
                //
                // origin.tracks.insert(track_id.clone(), track_id.clone());
                // let new_tracks = Arc::new(vec![track_id]);
                // for (_, participant) in &self.participants {
                //     let _ = participant.handle.add_tracks(new_tracks.clone()).await;
                // }
            }
            _ => todo!(),
        };
    }

    fn broadcast_event(&mut self, from: Arc<ParticipantId>, event: RoomEvent) {
        for (participant_id, meta) in &self.participants {
            // don't loopback to the sender, this will cause a deadlock!
            if *participant_id == from {
                continue;
            }

            let event = match meta.handle.control_tx.try_send(event.clone()) {
                Ok(_) => continue,
                Err(TrySendError::Closed(_)) => {
                    // the join task loop will run and cleanup
                    continue;
                }
                Err(TrySendError::Full(event)) => event,
            };

            let event = event.clone();
            let handle = meta.handle.clone();
            let participant_id = participant_id.clone();
            let task = Box::pin(async move {
                // TODO: pick a better timeout here
                let res = handle
                    .control_tx
                    .send_timeout(event, Duration::from_secs(1))
                    .await;

                match res {
                    Ok(_) => None,
                    // the join task loop will run and cleanup
                    Err(SendTimeoutError::Closed(_)) => None,
                    Err(SendTimeoutError::Timeout(_)) => Some(participant_id),
                }
            });
            self.event_queue.push(task);
        }
    }

    fn handle_participant_left(&mut self, participant_id: Arc<ParticipantId>) {
        // TODO: notify participant leaving
        // let Some(participant) = self.participants.remove(&participant_id) else {
        //     return;
        // };
        //
        // let tracks: Vec<Arc<TrackId>> = participant
        //     .tracks
        //     .into_values()
        //     .map(|t| t.meta.id.clone())
        //     .collect();
        // let tracks = Arc::new(tracks);
        // for (_, participant) in &self.participants {
        //     let _ = participant.handle.remove_tracks(tracks.clone()).await;
        // }
    }

    fn handle_data_message(&mut self, msg: RoomDataMessage) {
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
                for (participant_id, meta) in &self.participants {
                    if track_id.origin_participant == *participant_id {
                        continue;
                    }
                    let _ = meta.handle.forward_audio(track_id.clone(), media.clone());
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
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
            voice_ranker: VoiceRanker::default(),
            video_forwarding_rules: HashMap::new(),
            event_queue: FuturesUnordered::new(),
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
            .send(RoomControlMessage::TrackAdded(track))
            .await
    }

    pub async fn unpublish(
        &self,
        track_id: Arc<TrackId>,
    ) -> Result<(), SendError<RoomControlMessage>> {
        self.control_tx
            .send(RoomControlMessage::TrackRemoved(track_id))
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
