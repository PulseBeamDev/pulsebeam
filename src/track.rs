use std::{collections::BTreeMap, sync::Arc, time::Duration};

use str0m::media::MediaData;
use tokio::{
    sync::mpsc::{
        self,
        error::{SendError, TrySendError},
    },
    time::Instant,
};

use crate::{
    actor::Actor,
    entity::{ParticipantId, TrackId},
    message::{self, TrackIn},
    participant::ParticipantHandle,
};

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
}

/// Responsibilities:
/// * Represent a Single Published Track
/// * Manage Track Subscribers
/// * Store Subscriber Preferences: Keep track of the desired quality/layer (DesiredLayerInfo) requested by each subscriber.
/// * Receive Packet Notifications
/// * Filter & Forward Packet Notifications
/// * Route Publisher-Bound RTCP: Receive RTCP feedback (PLI, FIR, etc.) from subscriber and forward it to the publisher
pub struct TrackActor {
    meta: Arc<TrackIn>,
    data_receiver: mpsc::Receiver<TrackDataMessage>,
    control_receiver: mpsc::Receiver<TrackControlMessage>,
    origin: ParticipantHandle,
    subscribers: BTreeMap<Arc<ParticipantId>, ParticipantHandle>,
    last_keyframe_request: Instant,
}

impl Actor for TrackActor {
    type ID = Arc<TrackId>;

    fn kind(&self) -> &'static str {
        "track"
    }

    fn id(&self) -> Self::ID {
        self.meta.id.clone()
    }

    async fn run(&mut self) -> Result<(), crate::actor::ActorError> {
        loop {
            tokio::select! {
                Some(msg) = self.data_receiver.recv() => {
                    self.handle_data_message(msg);
                }

                Some(msg) = self.control_receiver.recv() => {
                    self.handle_control_message(msg);
                }

                else => break,
            }
        }
        Ok(())
    }
}

impl TrackActor {
    #[inline]
    fn handle_data_message(&mut self, msg: TrackDataMessage) {
        match msg {
            TrackDataMessage::ForwardMedia(data) => {
                for (_, sub) in &self.subscribers {
                    let _ = sub.forward_media(self.meta.clone(), data.clone());
                }
            }
            TrackDataMessage::KeyframeRequest(req) => {
                let now = Instant::now();
                let elapsed = now.duration_since(self.last_keyframe_request);
                if elapsed >= KEYFRAME_REQUEST_THROTTLE {
                    self.origin.request_keyframe(self.meta.id.clone(), req);
                    self.last_keyframe_request = now;
                }
            }
        }
    }

    fn handle_control_message(&mut self, msg: TrackControlMessage) {
        match msg {
            TrackControlMessage::Subscribe(participant) => {
                tracing::info!(participant_id=?participant.participant_id, "track subscribed");
                self.subscribers
                    .insert(participant.participant_id.clone(), participant);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrackHandle {
    pub data_sender: mpsc::Sender<TrackDataMessage>,
    pub control_sender: mpsc::Sender<TrackControlMessage>,
    pub meta: Arc<TrackIn>,
}

impl TrackHandle {
    pub fn new(origin: ParticipantHandle, meta: Arc<TrackIn>) -> (Self, TrackActor) {
        let (data_sender, data_receiver) = mpsc::channel(64);
        let (control_sender, control_receiver) = mpsc::channel(8);
        let handle = Self {
            data_sender,
            control_sender,
            meta: meta.clone(),
        };
        let actor = TrackActor {
            meta,
            data_receiver,
            control_receiver,
            origin,
            subscribers: BTreeMap::new(),
            // allow keyframe request immediately
            last_keyframe_request: Instant::now() - KEYFRAME_REQUEST_THROTTLE,
        };
        (handle, actor)
    }

    pub fn forward_media(
        &self,
        data: Arc<MediaData>,
    ) -> Result<(), TrySendError<TrackDataMessage>> {
        let res = self
            .data_sender
            .try_send(TrackDataMessage::ForwardMedia(data));

        if let Err(err) = &res {
            tracing::warn!("media packet is dropped: {err}");
        }

        res
    }

    pub async fn subscribe(
        &self,
        participant: ParticipantHandle,
    ) -> Result<(), SendError<TrackControlMessage>> {
        self.control_sender
            .send(TrackControlMessage::Subscribe(participant))
            .await
    }

    pub fn request_keyframe(&self, req: message::KeyframeRequest) {
        // Keyframe request is lossy. The receiver is responsible in resending.
        // There can be many in-flight keyframe requests, the track actor may throttle
        // the requests.
        if let Err(err) = self
            .data_sender
            .try_send(TrackDataMessage::KeyframeRequest(req))
        {
            tracing::warn!("keyframe request is dropped by the track actor: {err}");
        }
    }
}
