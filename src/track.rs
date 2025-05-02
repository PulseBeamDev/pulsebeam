use std::{collections::BTreeMap, panic::AssertUnwindSafe, sync::Arc};

use futures::FutureExt;
use str0m::media::Mid;
use tokio::sync::mpsc;

use crate::{
    message::{ActorId, ActorResult, ActorResultWithId, PeerId, TrackIn},
    peer::PeerHandle,
};

#[derive(Debug, thiserror::Error)]
pub enum TrackError {}

#[derive(Debug)]
pub enum TrackMessage {}

/// Responsibilities:
/// * Represent a Single Published Track
/// * Manage Track Subscribers
/// * Store Subscriber Preferences: Keep track of the desired quality/layer (DesiredLayerInfo) requested by each subscriber.
/// * Receive Packet Notifications
/// * Filter & Forward Packet Notifications
/// * Route Publisher-Bound RTCP: Receive RTCP feedback (PLI, FIR, etc.) from subscriber and forward it to the publisher
pub struct TrackActor {
    meta: Arc<TrackIn>,
    receiver: mpsc::Receiver<TrackMessage>,
    origin: PeerHandle,
    subscribers: BTreeMap<Arc<PeerId>, PeerHandle>,
}

impl TrackActor {
    pub async fn run<I: ActorId>(self, id: I) -> I {
        match AssertUnwindSafe(self.run_inner()).catch_unwind().await {
            Ok(Ok(())) => {
                tracing::info!(?id, "track actor exited.");
            }
            Ok(Err(err)) => {
                tracing::warn!(?id, "track actor exited with an error: {err}");
            }
            Err(err) => {
                tracing::error!(?id, "track actor panicked: {:?}", err);
            }
        };
        id
    }

    async fn run_inner(mut self) -> ActorResult {
        while let Some(msg) = self.receiver.recv().await {
            match msg {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct TrackHandle {
    sender: mpsc::Sender<TrackMessage>,
    meta: Arc<TrackIn>,
}

impl TrackHandle {
    pub fn new(origin: PeerHandle, meta: Arc<TrackIn>) -> (Self, TrackActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self {
            sender,
            meta: meta.clone(),
        };
        let actor = TrackActor {
            meta,
            receiver,
            origin,
            subscribers: BTreeMap::new(),
        };
        (handle, actor)
    }
}
