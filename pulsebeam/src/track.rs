use std::{collections::BTreeMap, sync::Arc, time::Duration};

use str0m::media::MediaData;
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, TrackId},
    message::{self, TrackMeta},
    participant::{self, ParticipantHandle},
};
use pulsebeam_runtime::actor;

const KEYFRAME_REQUEST_THROTTLE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum TrackError {}

#[derive(Debug)]
pub enum TrackDataMessage {
    ForwardMedia(Arc<MediaData>),
    KeyframeRequest(message::KeyframeRequest),
}

#[derive(Debug)]
pub enum TrackControlMessage {
    Subscribe(ParticipantHandle),
    Unsubscribe(Arc<ParticipantId>),
}

/// Responsibilities:
/// * Represent a Single Published Track
/// * Manage Track Subscribers
/// * Store Subscriber Preferences: Keep track of the desired quality/layer (DesiredLayerInfo) requested by each subscriber.
/// * Receive Packet Notifications
/// * Filter & Forward Packet Notifications
/// * Route Publisher-Bound RTCP: Receive RTCP feedback (PLI, FIR, etc.) from subscriber and forward it to the publisher
pub struct TrackActor {
    meta: Arc<TrackMeta>,
    origin: ParticipantHandle,
    subscribers: BTreeMap<Arc<ParticipantId>, ParticipantHandle>,
    last_keyframe_request: Instant,
}

impl actor::Actor for TrackActor {
    type HighPriorityMsg = TrackControlMessage;
    type LowPriorityMsg = TrackDataMessage;
    type Meta = Arc<TrackMeta>;
    type ObservableState = ();

    fn meta(&self) -> Self::Meta {
        self.meta.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        match msg {
            TrackControlMessage::Subscribe(participant) => {
                tracing::info!(participant_id=?participant, "track subscribed");
                self.subscribers
                    .insert(participant.meta.clone(), participant);
            }
            TrackControlMessage::Unsubscribe(participant_id) => {
                // TODO: handle unsubscribe
            }
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
        match msg {
            TrackDataMessage::ForwardMedia(data) => {
                for sub in self.subscribers.values() {
                    let _ = sub.try_send_low(participant::ParticipantDataMessage::ForwardMedia(
                        self.meta.clone(),
                        data.clone(),
                    ));
                }
            }
            TrackDataMessage::KeyframeRequest(req) => {
                let now = Instant::now();
                let elapsed = now.duration_since(self.last_keyframe_request);
                if elapsed >= KEYFRAME_REQUEST_THROTTLE {
                    let _ = self.origin.try_send_low(
                        participant::ParticipantDataMessage::KeyframeRequest(
                            self.meta.id.clone(),
                            req,
                        ),
                    );
                    self.last_keyframe_request = now;
                }
            }
        }
    }
}

impl TrackActor {
    pub fn new(origin: ParticipantHandle, meta: Arc<TrackMeta>) -> Self {
        Self {
            meta,
            origin,
            subscribers: BTreeMap::new(),
            // allow keyframe request immediately
            last_keyframe_request: Instant::now() - KEYFRAME_REQUEST_THROTTLE,
        }
    }
}

pub type TrackHandle = actor::ActorHandle<TrackActor>;
