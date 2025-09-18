use std::collections::HashMap;
use std::sync::Arc;

use prost::Message;
use str0m::{
    channel::ChannelData,
    media::{Direction, KeyframeRequest, MediaAdded, Mid},
    net::Transmit,
};

use super::state::{MidOutSlot, ParticipantState};
use crate::{
    entity::{ParticipantId, TrackId},
    message::{self, TrackMeta},
    proto::sfu,
    track,
};

/// Represents an "effect" or a command that the pure core logic
/// needs the actor shell to execute in the outside world.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ParticipantEffect {
    Transmit(Transmit),
    RequestKeyframeToClient(KeyframeRequest),
    RequestKeyframeToTrack(track::TrackHandle, message::KeyframeRequest),
    SubscribeToTrack(track::TrackHandle),
    SendRpc(Vec<u8>),
    SpawnTrack(Arc<TrackMeta>),
    Disconnect,
}

/// Owns the participant's state and contains all business logic for the CONTROL PLANE.
pub struct ParticipantCore {
    pub state: ParticipantState,
}

impl ParticipantCore {
    pub fn new(participant_id: Arc<ParticipantId>) -> Self {
        Self {
            state: ParticipantState::new(participant_id),
        }
    }

    // --- Ingress Path Helpers (for Actor) ---

    #[inline]
    pub fn get_published_track(&self, mid: Mid) -> Option<&track::TrackHandle> {
        self.state.get_published_track(mid)
    }

    #[inline]
    pub fn get_published_track_mut(&mut self, mid: Mid) -> Option<&mut track::TrackHandle> {
        self.state.get_published_track_mut(mid)
    }

    // --- Egress Path Helpers (for Actor) ---

    #[inline]
    pub fn get_egress_mid(&self, track_id: &Arc<TrackId>) -> Option<Mid> {
        self.state.get_egress_mid(track_id)
    }

    // --- Control Plane: WebRTC Events ---

    pub fn handle_media_added(&mut self, media: MediaAdded) -> Vec<ParticipantEffect> {
        tracing::info!(?media, "Core: Handling MediaAdded");
        let mut effects = vec![];
        match media.direction {
            Direction::RecvOnly => {
                // Client is publishing a track.
                let track_id = Arc::new(TrackId::new(self.state.participant_id.clone(), media.mid));
                let track_meta = Arc::new(TrackMeta {
                    id: track_id,
                    kind: media.kind,
                    simulcast_rids: media.simulcast.map(|s| s.recv),
                });
                effects.push(ParticipantEffect::SpawnTrack(track_meta));
            }
            Direction::SendOnly => {
                // Client opened a subscription slot.
                let subscribed_map = match media.kind {
                    str0m::media::MediaKind::Video => &mut self.state.subscribed_video_tracks,
                    str0m::media::MediaKind::Audio => &mut self.state.subscribed_audio_tracks,
                };
                subscribed_map.insert(media.mid, MidOutSlot { track_id: None });
                // Inserting a new slot means we might need to auto-subscribe.
                self.state.should_resync = true;
            }
            _ => effects.push(ParticipantEffect::Disconnect),
        }

        effects
    }

    pub fn handle_keyframe_request_from_client(
        &self,
        req: KeyframeRequest,
    ) -> Vec<ParticipantEffect> {
        // Find which track this client is subscribed to on the given MID.
        let Some(MidOutSlot {
            track_id: Some(track_id),
            ..
        }) = self.state.subscribed_video_tracks.get(&req.mid)
        else {
            return vec![];
        };

        // Find the TrackHandle for that track.
        let Some(track_out) = self.state.available_video_tracks.get(&track_id.internal) else {
            return vec![];
        };

        // Forward the request to the TrackActor (which will forward it to the publisher).
        vec![ParticipantEffect::RequestKeyframeToTrack(
            track_out.track.clone(),
            req.into(),
        )]
    }

    pub fn handle_rpc_data(&mut self, data: ChannelData) -> Vec<ParticipantEffect> {
        let Ok(msg) = sfu::ClientMessage::decode(data.data.as_slice()) else {
            tracing::warn!("Invalid RPC format");
            return vec![];
        };

        if let Some(payload) = msg.payload {
            match payload {
                sfu::client_message::Payload::Subscribe(sub) => {
                    tracing::info!(?sub, "Client Subscribe request (unimplemented)");
                    // TODO: Implement explicit subscription.
                }
                sfu::client_message::Payload::Unsubscribe(unsub) => {
                    tracing::info!(?unsub, "Client Unsubscribe request (unimplemented)");
                    // TODO: Implement explicit unsubscription.
                }
            }
        }
        vec![]
    }

    // --- Control Plane: Actor Messages ---

    pub fn handle_tracks_update(&mut self, tracks: &HashMap<Arc<TrackId>, track::TrackHandle>) {
        self.state.handle_new_tracks(tracks);
    }

    pub fn handle_tracks_removed(&mut self, tracks: &HashMap<Arc<TrackId>, track::TrackHandle>) {
        self.state.remove_available_tracks(tracks);
    }

    pub fn handle_track_unpublished(&mut self, track_meta: Arc<TrackMeta>) {
        tracing::info!(track_id=%track_meta.id, "Core: Local track unpublished");
        let published_map = match track_meta.kind {
            str0m::media::MediaKind::Video => &mut self.state.published_video_tracks,
            str0m::media::MediaKind::Audio => &mut self.state.published_audio_tracks,
        };
        published_map.remove(&track_meta.id.origin_mid);
    }

    pub fn handle_keyframe_request_from_track(
        &self,
        track_id: Arc<TrackId>,
        req: message::KeyframeRequest,
    ) -> Vec<ParticipantEffect> {
        // A subscribed track needs a keyframe. Send the request back to the client who published it.
        vec![ParticipantEffect::RequestKeyframeToClient(
            KeyframeRequest {
                mid: track_id.origin_mid,
                kind: req.kind,
                rid: req.rid,
            },
        )]
    }

    // --- Periodic State Sync ---

    pub fn handle_resync_check(&mut self) -> Vec<ParticipantEffect> {
        if !self.state.should_resync {
            return vec![];
        }
        self.state.should_resync = false;

        let mut effects = vec![];

        // 1. Auto-Subscribe Logic (Video only for brevity)
        {
            let mut available_tracks_iter = self.state.available_video_tracks.values_mut();

            for (slot_mid, slot) in self.state.subscribed_video_tracks.iter_mut() {
                if slot.track_id.is_some() {
                    continue; // Slot already filled
                }

                if let Some(track_out) = available_tracks_iter.next() {
                    if track_out.mid.is_some() {
                        // This should ideally not happen if iter is correct, but safety check.
                        continue;
                    }

                    tracing::info!(track_id=%track_out.track.meta.id, mid=?slot_mid, "Auto-subscribing");

                    effects.push(ParticipantEffect::SubscribeToTrack(track_out.track.clone()));

                    track_out.mid.replace(*slot_mid);
                    slot.track_id.replace(track_out.track.meta.id.clone());
                } else {
                    // No more tracks available to fill remaining slots.
                    break;
                }
            }
        }

        // TODO:
        // 2. RPC Sync Logic (Tell client about updates)
        // if !self.state.pending_published_tracks.is_empty()
        //     || !self.state.pending_unpublished_tracks.is_empty()
        // {
        //     let payload = sfu::server_message::Payload::Sync(sfu::Sync {
        //         published_tracks: self.state.pending_published_tracks.drain(..).collect(),
        //         unpublished_tracks: self.state.pending_unpublished_tracks.drain(..).collect(),
        //     });
        //
        //     let encoded = sfu::ServerMessage {
        //         payload: Some(payload),
        //     }
        //     .encode_to_vec();
        //     effects.push(ParticipantEffect::SendRpc(encoded));
        // }
        //
        effects
    }
}

// -------------------------------------------------------------------------------------------------
// Unit Tests
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant::state::{MidOutSlot, TrackOut};
    // Import our beautiful, ergonomic test helper
    use crate::track::test::new_fake_track;
    use crate::track::{TrackControlMessage, TrackDataMessage};
    use std::collections::HashMap;
    use str0m::media::{KeyframeRequestKind, MediaKind, Simulcast};

    #[test]
    fn handle_media_added_recv_only_produces_spawn_track_effect() {
        let pid = Arc::new(ParticipantId::new());
        let mut core = ParticipantCore::new(pid.clone());
        let media = MediaAdded {
            mid: Mid::new(),
            direction: Direction::RecvOnly,
            kind: MediaKind::Video,
            simulcast: Some(Simulcast {
                recv: vec![],
                send: vec![],
            }),
        };

        let effects = core.handle_media_added(media);
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], ParticipantEffect::SpawnTrack(_)));
        assert!(!core.state.should_resync);

        let media = MediaAdded {
            mid: Mid::new(),
            direction: Direction::SendOnly,
            kind: MediaKind::Video,
            simulcast: Some(Simulcast {
                recv: vec![],
                send: vec![],
            }),
        };
        let effects = core.handle_media_added(media);
        assert!(effects.is_empty());
        assert!(core.state.should_resync);
    }

    #[test]
    fn handle_media_added_send_only_updates_state_with_subscription_slot() {
        let mut core = ParticipantCore::new(Arc::new(ParticipantId::new()));
        let mid = Mid::new();
        let media = MediaAdded {
            mid,
            direction: Direction::SendOnly,
            kind: MediaKind::Audio,
            simulcast: None,
        };

        let effects = core.handle_media_added(media);

        assert!(effects.is_empty());
        assert!(core.state.should_resync);
        assert!(core.state.subscribed_audio_tracks.contains_key(&mid));
    }

    #[tokio::test]
    #[ignore]
    async fn auto_subscribe_fills_slot_and_updates_track_actor_state() {
        // ARRANGE: A participant and a fake remote track.
        let my_pid = Arc::new(ParticipantId::new());
        // let my_fake = FakeActorBuilder::<ParticipantActor>::new(my_pid.clone()).build();
        let mut core = ParticipantCore::new(my_pid.clone());

        // The story: "A remote track exists. When it receives a Subscribe message,
        // it adds the participant's ID to its list of subscribers."
        let (remote_track_handle, remote_track) =
            new_fake_track(Arc::new(ParticipantId::new()), MediaKind::Video)
                .on_high(|state, msg| {
                    if let TrackControlMessage::Subscribe(p) = msg {
                        state.subscribers.push(p.meta.clone());
                    }
                })
                .build();
        remote_track.assert_state(|s| assert!(s.subscribers.is_empty()));

        // The room announces the track is available, and the participant opens a sub slot.
        core.handle_tracks_update(&HashMap::from([(
            remote_track_handle.meta.id.clone(),
            remote_track_handle,
        )]));
        core.state
            .subscribed_video_tracks
            .insert(Mid::new(), MidOutSlot { track_id: None });
        core.state.should_resync = true;

        // ACT: The core's resync logic runs and decides to subscribe.
        let effects = core.handle_resync_check();
        // TODO:
        // for effect in effects {
        //     if let ParticipantEffect::SubscribeToTrack(mut handle) = effect {
        //         handle
        //             .send_high(TrackControlMessage::Subscribe(my_fake.0))
        //             .await
        //             .unwrap();
        //     }
        // }
        tokio::task::yield_now().await; // Allow the fake to process the message.

        // ASSERT: Verify the final state of the fake actor.
        // "The remote track's state now correctly shows our participant as a subscriber."
        remote_track.assert_state(|s| {
            assert_eq!(s.subscribers.len(), 1);
            assert_eq!(s.subscribers[0], my_pid);
        });
        assert!(!core.state.should_resync);
    }

    #[tokio::test]
    async fn tracks_removed_frees_participant_slot() {
        // ARRANGE: A participant subscribed to a fake track.
        let mut core = ParticipantCore::new(Arc::new(ParticipantId::new()));
        let (remote_track_handle, _remote_track_state) =
            new_fake_track(Arc::new(ParticipantId::new()), MediaKind::Video).build();
        let track_id = remote_track_handle.meta.id.clone();
        let slot_mid = Mid::new();
        core.state.available_video_tracks.insert(
            track_id.internal.clone(),
            TrackOut {
                track: remote_track_handle.clone(),
                mid: Some(slot_mid),
            },
        );
        core.state.subscribed_video_tracks.insert(
            slot_mid,
            MidOutSlot {
                track_id: Some(track_id.clone()),
            },
        );

        // ACT: The room announces the track has been removed.
        core.handle_tracks_removed(&HashMap::from([(track_id, remote_track_handle)]));

        // ASSERT: The participant's state should change.
        let slot = &core.state.subscribed_video_tracks[&slot_mid];
        assert!(slot.track_id.is_none(), "Subscription slot should be freed");
        assert!(core.state.should_resync);
    }

    #[tokio::test]
    async fn keyframe_request_from_client_updates_track_actor_state() {
        // ARRANGE: A participant subscribed to a fake remote track.
        let mut core = ParticipantCore::new(Arc::new(ParticipantId::new()));

        // The story: "A remote track exists. When it receives a KeyframeRequest,
        // it should increment its request counter."
        let (remote_track_handle, remote_track) =
            new_fake_track(Arc::new(ParticipantId::new()), MediaKind::Video)
                .on_low(|state, msg| {
                    if let TrackDataMessage::KeyframeRequest(_) = msg {
                        state.keyframe_requests_received += 1;
                    }
                })
                .build();
        remote_track.assert_state(|s| assert_eq!(s.keyframe_requests_received, 0));

        // Set up the subscription state.
        let track_id = remote_track_handle.meta.id.clone();
        let slot_mid = Mid::new();
        core.state.available_video_tracks.insert(
            track_id.internal.clone(),
            TrackOut {
                track: remote_track_handle.clone(),
                mid: Some(slot_mid),
            },
        );
        core.state.subscribed_video_tracks.insert(
            slot_mid,
            MidOutSlot {
                track_id: Some(track_id),
            },
        );

        // ACT: The client sends a keyframe request.
        let keyframe_req_event = KeyframeRequest {
            mid: slot_mid,
            kind: KeyframeRequestKind::Pli,
            rid: None,
        };
        let effects = core.handle_keyframe_request_from_client(keyframe_req_event);
        for effect in effects {
            if let ParticipantEffect::RequestKeyframeToTrack(mut handle, req) = effect {
                handle
                    .try_send_low(TrackDataMessage::KeyframeRequest(req))
                    .unwrap();
            }
        }
        tokio::task::yield_now().await; // Allow the fake to process.

        // ASSERT: "The track's keyframe request counter should now be 1."
        remote_track.assert_state(|s| assert_eq!(s.keyframe_requests_received, 1));
    }
}
