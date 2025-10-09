use std::{collections::BTreeMap, sync::Arc, time::Duration};

use str0m::{
    media::{KeyframeRequestKind, Rid},
    rtp::RtpPacket,
};
use tokio::time::Instant;

use crate::{
    entity::ParticipantId,
    message::{self, TrackMeta},
    participant::{self, ParticipantHandle},
};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, rt};

const KEYFRAME_REQUEST_THROTTLE: Duration = Duration::from_secs(1);
// Common simulcast RIDs (quarter, half, full)
pub const RID_Q: Rid = Rid::from_array(*b"q\0\0\0\0\0\0\0");
pub const RID_H: Rid = Rid::from_array(*b"h\0\0\0\0\0\0\0");
pub const RID_F: Rid = Rid::from_array(*b"f\0\0\0\0\0\0\0");

#[derive(Debug, thiserror::Error)]
pub enum TrackError {}

#[derive(Debug)]
pub enum TrackDataMessage {
    ForwardRtp(Arc<RtpPacket>),
    KeyframeRequest(message::KeyframeRequest),
}

#[derive(Debug)]
pub enum TrackControlMessage {
    Subscribe(ParticipantHandle),
    Unsubscribe(Arc<ParticipantId>),
}

pub struct TrackMessageSet;

impl actor::MessageSet for TrackMessageSet {
    type HighPriorityMsg = TrackControlMessage;
    type LowPriorityMsg = TrackDataMessage;
    type Meta = Arc<TrackMeta>;
    type ObservableState = ();
}

/// Responsibilities:
/// * Represent a Single Published Track
/// * Manage Track Subscribers
/// * Store Subscriber Preferences: Keep track of the desired quality/layer (DesiredLayerInfo) requested by each subscriber.
/// * Receive Packet Notifications
/// * Filter & Forward Packet Notifications
/// * Route Publisher-Bound RTCP: Receive RTCP feedback (PLI, FIR, etc.) from subscriber and forward it to the publisher
/// * Estimate bandwidth per stream
/// * Receive bandwidth budget per subscriber
/// * Maintain bitrate to meet each subscriber's budget by switching layers and pacing
pub struct TrackActor {
    meta: Arc<TrackMeta>,
    origin: ParticipantHandle,
    subscribers: BTreeMap<Arc<ParticipantId>, ParticipantHandle>,
    last_keyframe_request: Instant,

    pinned_rid: Option<Rid>,
}

impl actor::Actor<TrackMessageSet> for TrackActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn meta(&self) -> Arc<TrackMeta> {
        self.meta.clone()
    }

    fn get_observable_state(&self) {}

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<TrackMessageSet>,
        msg: TrackControlMessage,
    ) -> () {
        match msg {
            TrackControlMessage::Subscribe(participant) => {
                tracing::info!(participant_id=?participant, "track subscribed");
                self.subscribers
                    .insert(participant.meta.clone(), participant);
                self.request_keyframe(message::KeyframeRequest {
                    rid: self.pinned_rid,
                    kind: KeyframeRequestKind::Pli,
                });
            }
            TrackControlMessage::Unsubscribe(participant_id) => {
                self.subscribers.remove(&participant_id);
            }
        }
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<TrackMessageSet>,
        msg: TrackDataMessage,
    ) -> () {
        match msg {
            TrackDataMessage::ForwardRtp(rtp) => {
                // TODO: adjust streams based on subscribers
                if rtp.header.ext_vals.rid != self.pinned_rid {
                    return;
                }

                let mut to_remove = Vec::new();
                let mut detected_drop = false;
                for (participant_id, sub) in self.subscribers.iter_mut() {
                    tracing::trace!("forwarded media: track -> participant");

                    // let res = sub
                    //     .send_low(participant::ParticipantDataMessage::ForwardRtp(
                    //         self.meta.clone(),
                    //         rtp.clone(),
                    //     ))
                    //     .await;
                    //
                    // if let Err(mailbox::SendError(_)) = res {
                    //     // TODO: should this be a part of unsubscribe instead?
                    //     to_remove.push(participant_id.clone());
                    // }
                    let res = sub.try_send_low(participant::ParticipantDataMessage::ForwardRtp(
                        self.meta.clone(),
                        rtp.clone(),
                    ));
                    // This gets triggered when a participant actor leaves
                    match res {
                        Err(mailbox::TrySendError::Closed(_)) => {
                            to_remove.push(participant_id.clone());
                        }
                        Err(mailbox::TrySendError::Full(_)) => {
                            detected_drop = true;
                            tracing::warn!("participant queue is full, dropping");
                        }
                        Ok(_) => {}
                    }
                }

                for key in to_remove {
                    self.subscribers.remove(&key);
                }

                if detected_drop {
                    rt::yield_now().await;
                }
            }
            TrackDataMessage::KeyframeRequest(req) => {
                self.request_keyframe(req);
            }
        }
    }
}

pub fn pin_rid(simulcast_rids: &Option<Vec<Rid>>) -> Option<Rid> {
    if let Some(rids) = simulcast_rids {
        for rid in rids {
            if rid.starts_with('f') {
                return Some(*rid);
            }
        }
        None
    } else {
        None
    }
}

impl TrackActor {
    pub fn new(origin: ParticipantHandle, meta: Arc<TrackMeta>) -> Self {
        let pinned_rid = pin_rid(&meta.simulcast_rids);
        tracing::info!("selected: {pinned_rid:?}");

        Self {
            meta,
            origin,
            subscribers: BTreeMap::new(),
            // allow keyframe request immediately
            last_keyframe_request: Instant::now() - KEYFRAME_REQUEST_THROTTLE,
            pinned_rid,
        }
    }

    pub fn request_keyframe(&mut self, mut req: message::KeyframeRequest) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_keyframe_request);
        req.rid = self.pinned_rid;
        if elapsed >= KEYFRAME_REQUEST_THROTTLE {
            let _ = self
                .origin
                .try_send_low(participant::ParticipantDataMessage::KeyframeRequest(
                    self.meta.id.clone(),
                    req,
                ));
            self.last_keyframe_request = now;
        }
    }
}

pub type TrackHandle = actor::ActorHandle<TrackMessageSet>;

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::{
        entity,
        message::TrackMeta,
        test_utils,
        track::{self, TrackMessageSet},
    };
    use pulsebeam_runtime::actor::{Actor, ActorContext};
    use std::sync::{Arc, Mutex};
    use str0m::media::{MediaKind, Mid};

    /// A fake TrackActor implementation that records incoming state and messages.
    /// Useful for testing components that interact with tracks.
    pub struct FakeTrackActor {
        pub meta: Arc<TrackMeta>,
        pub received_high: Arc<Mutex<Vec<TrackControlMessage>>>,
        pub received_low: Arc<Mutex<Vec<TrackDataMessage>>>,
    }

    impl Actor<TrackMessageSet> for FakeTrackActor {
        fn meta(&self) -> Arc<TrackMeta> {
            self.meta.clone()
        }

        fn get_observable_state(&self) {}

        async fn on_high_priority(
            &mut self,
            _ctx: &mut ActorContext<TrackMessageSet>,
            msg: TrackControlMessage,
        ) {
            self.received_high.lock().unwrap().push(msg);
        }

        async fn on_low_priority(
            &mut self,
            _ctx: &mut ActorContext<TrackMessageSet>,
            msg: TrackDataMessage,
        ) {
            self.received_low.lock().unwrap().push(msg);
        }
    }

    impl FakeTrackActor {
        pub fn new(meta: Arc<TrackMeta>) -> Self {
            Self {
                meta,
                received_high: Arc::new(Mutex::new(Vec::new())),
                received_low: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    pub fn spawn_fake(
        kind: MediaKind,
    ) -> (
        track::TrackHandle,
        actor::JoinHandle<track::TrackMessageSet>,
    ) {
        let participant_id = test_utils::create_participant_id();
        let mid = Mid::new();
        let id = Arc::new(entity::TrackId::new(participant_id, mid));
        let meta = Arc::new(TrackMeta {
            id,
            kind,
            simulcast_rids: None,
        });
        let fake = track::test::FakeTrackActor::new(meta);
        actor::spawn_default(fake)
    }
}
