use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use crate::{
    control::{
        controller::{ControllerCommand, ControllerError, ParticipantState},
        negotiator::Negotiator,
        registry::RoomRegistry,
        room::Room,
    },
    entity::{ParticipantId, RoomId},
    participant::ParticipantConfig,
    shard::worker::ShardEvent,
};
use futures_lite::StreamExt;
use indexmap::IndexMap;
use str0m::{
    Candidate, Rtc, RtcConfig, RtcError,
    change::{SdpAnswer, SdpOffer},
    format::{Codec, FormatParams},
    media::{Direction, Frequency, MediaKind, Pt},
};
use tokio::time::Instant;

/// Maximum participants allowed per "slot" before hashing to a new shard epoch.
const MAX_PARTICIPANTS_PER_SHARD_SLOT: usize = 16;
const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub enum ControllerEvent {}

pub struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
}

impl ControllerEventQueue {
    pub fn default() -> Self {
        Self {
            queue: VecDeque::with_capacity(1024),
        }
    }

    pub fn push(&mut self, ev: ControllerEvent) {
        self.queue.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }
}

struct ParticipantMeta {
    shard_id: usize,
    room_id: RoomId,
}

pub struct ControllerCore {
    negotiator: Negotiator,
    registry: RoomRegistry,
}

impl ControllerCore {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            negotiator: Negotiator::new(candidates),
            registry: RoomRegistry::new(),
        }
    }

    pub async fn next_expired(&mut self) {
        self.registry.next_expired().await;
    }

    pub fn process_shard_event(&mut self, ev: ShardEvent, eq: &mut ControllerEventQueue) {
        match ev {
            ShardEvent::TrackPublished(track) => {
                let origin = track.meta.origin;
                let Some(room) = self.registry.room_mut_for(&track.meta.origin) else {
                    return;
                };

                // TODO: make room shard aware?
                let mut shard_ids: IndexMap<usize, ()> = IndexMap::new();
                for participant_id in room.participants_iter() {
                    if *participant_id == origin {
                        continue;
                    }
                    if let Some(p) = self.registry.get(participant_id) {
                        shard_ids.entry(p.shard_id).or_default();
                    }
                }

                tracing::info!(
                    track = %track.meta.id,
                    %origin,
                    room_id = ?room_id,
                    shard_count = shard_ids.len(),
                    "fanning out track to shards"
                );
                room.publish_track(track.clone());
                for (shard_id, _) in shard_ids {
                    self.router
                        .send(shard_id, ShardCommand::PublishTrack(track.clone(), room_id))
                        .await;
                }
            }

            ShardEvent::ParticipantExited(participant_id) => {
                self.delete_participant(&participant_id).await;
            }
            ShardEvent::KeyframeRequest(req) => {
                let meta = self.registry.get(&req.origin).or_else(|| {
                    tracing::warn!(origin = %req.origin, track = ?req.stream_id.0, "KeyframeRequest: origin participant not found in controller");
                    None
                })?;
                self.router
                    .send(meta.shard_id, ShardCommand::RequestKeyframe(req))
                    .await;
            }
        }

        Some(())
    }

    pub fn process_command(&mut self, cmd: ControllerCommand, eq: &mut ControllerEventQueue) {
        match cmd {
            ControllerCommand::CreateParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .await
                    .map(|res| CreateParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }

            ControllerCommand::DeleteParticipant(m) => {
                self.delete_participant(&m.participant_id).await;
            }
            ControllerCommand::PatchParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .await
                    .map(|res| PatchParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }
        }
    }

    pub async fn handle_create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
        eq: &mut ControllerEventQueue,
    ) -> Result<SdpAnswer, ControllerError> {
        let answer = self.create_participant(state, offer, eq)?;
        Ok(answer)
    }

    fn routing_key_for(&self, room_id: &RoomId) -> Option<String> {
        let room = self.registry.get_room(room_id)?;
        let epoch = room.participant_count() / MAX_PARTICIPANTS_PER_SHARD_SLOT;
        let key = format!("{}-{}", room_id, epoch);
        Some(key)
    }

    fn add_participant(&mut self, participant: ParticipantConfig) {}

    // step 1: validations
    // step 2: find a routing
    // step 3: add participant

    async fn create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let (mut rtc, answer) = self.create_answer(offer)?;
        let room = self.registry.get_or_create_room(state.room_id);
        let tracks = room.tracks_for(&state.participant_id);
        let participant_id = state.participant_id;
        let cfg = ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id,
            rtc,
            available_tracks: tracks.cloned().collect(),
        };
        Ok(answer)
    }

    async fn delete_participant(&mut self, participant_id: &ParticipantId) {
        let Some(meta) = self.participants.remove(participant_id) else {
            return;
        };
        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remove_participant(participant_id);
            if room.participant_count() == 0 {
                self.sweeper.insert(meta.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        self.router
            .broadcast(|| ShardCommand::UnregisterParticipant {
                participant_id: *participant_id,
            })
            .await;
    }
}
