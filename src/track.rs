use std::{collections::BTreeMap, sync::Arc};

use str0m::media::Mid;
use tokio::sync::mpsc;

use crate::{
    message::{ActorResult, PeerId},
    peer::PeerHandle,
};

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
    receiver: mpsc::Receiver<TrackMessage>,
    origin: Arc<PeerId>,
    mid: Mid,
    subscribers: BTreeMap<Arc<PeerId>, PeerHandle>,
}

impl TrackActor {
    pub async fn run(mut self) -> ActorResult {
        while let Some(msg) = self.receiver.recv().await {
            match msg {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct TrackHandle {
    sender: mpsc::Sender<TrackMessage>,
}

impl TrackHandle {
    pub fn new(origin: Arc<PeerId>, mid: Mid) -> (Self, TrackActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self { sender };
        let actor = TrackActor {
            receiver,
            origin,
            mid,
            subscribers: BTreeMap::new(),
        };
        (handle, actor)
    }
}
