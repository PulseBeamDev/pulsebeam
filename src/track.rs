use std::{collections::BTreeMap, panic::AssertUnwindSafe, sync::Arc};

use futures::FutureExt;
use str0m::media::MediaData;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    entity::ParticipantId,
    message::{ActorResult, TrackIn},
    participant::ParticipantHandle,
};

#[derive(Debug, thiserror::Error)]
pub enum TrackError {}

#[derive(Debug)]
pub enum TrackDataMessage {
    ForwardMedia(Arc<MediaData>),
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
}

impl TrackActor {
    pub async fn run(self) {
        match AssertUnwindSafe(self.run_inner()).catch_unwind().await {
            Ok(Ok(())) => {
                tracing::info!("track actor exited.");
            }
            Ok(Err(err)) => {
                tracing::warn!("track actor exited with an error: {err}");
            }
            Err(err) => {
                tracing::error!("track actor panicked: {:?}", err);
            }
        };
    }

    async fn run_inner(mut self) -> ActorResult {
        loop {
            tokio::select! {
                biased;

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

    #[inline]
    fn handle_data_message(&self, msg: TrackDataMessage) {
        match msg {
            TrackDataMessage::ForwardMedia(data) => {
                for (_, sub) in &self.subscribers {
                    sub.forward_media(self.meta.clone(), data.clone());
                }
            }
        }
    }

    fn handle_control_message(&mut self, msg: TrackControlMessage) {
        match msg {
            TrackControlMessage::Subscribe(participant) => {
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
        let (control_sender, control_receiver) = mpsc::channel(1);
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
        };
        (handle, actor)
    }

    pub fn forward_media(
        &self,
        data: Arc<MediaData>,
    ) -> Result<(), TrySendError<TrackDataMessage>> {
        self.data_sender
            .try_send(TrackDataMessage::ForwardMedia(data))
    }
}
