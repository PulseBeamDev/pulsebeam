use std::collections::VecDeque;

use str0m::media::{KeyframeRequestKind, MediaKind};
use tokio::time::Instant;

use crate::entity::ParticipantId;
use crate::rtp::RtpPacket;
use crate::track::{GlobalKeyframeRequest, StreamId, Track, TrackLayer};

pub struct RtpEvent {
    pub stream_id: StreamId,
    pub pkt: RtpPacket,
}

pub enum ParticipantEvent {
    Topology(TopologyEvent),
    Timer(TimerEvent),
    Lifecycle(LifecycleEvent),
    Control(ControlEvent),
}

pub enum TopologyEvent {
    StreamSubscribed {
        shard_id: usize,
        participant_id: ParticipantId,
        stream_id: StreamId,
        kind: MediaKind,
    },
    StreamUnsubscribed {
        shard_id: usize,
        participant_id: ParticipantId,
        stream_id: StreamId,
    },
}

pub enum TimerEvent {
    DeadlineUpdated {
        at: Instant,
        participant_id: ParticipantId,
    },
}

pub enum LifecycleEvent {
    Exited { participant_id: ParticipantId },
}

pub enum ControlEvent {
    TrackPublished(Track),
    KeyframeRequested(GlobalKeyframeRequest),
}

pub struct EventQueue<'a> {
    id: &'a ParticipantId,
    queue: &'a mut VecDeque<ParticipantEvent>,
    rtp_queue: &'a mut VecDeque<RtpEvent>,
}

impl<'a> EventQueue<'a> {
    pub fn new(
        id: &'a ParticipantId,
        queue: &'a mut VecDeque<ParticipantEvent>,
        rtp_queue: &'a mut VecDeque<RtpEvent>,
    ) -> Self {
        Self {
            id,
            queue,
            rtp_queue,
        }
    }

    pub fn subscribe(&mut self, layer: &TrackLayer) {
        self.queue.push_back(ParticipantEvent::Topology(
            TopologyEvent::StreamSubscribed {
                shard_id: layer.meta.shard_id,
                participant_id: *self.id,
                stream_id: layer.stream_id(),
                kind: layer.meta.kind,
            },
        ));
    }

    pub fn unsubscribe(&mut self, layer: &TrackLayer) {
        self.queue.push_back(ParticipantEvent::Topology(
            TopologyEvent::StreamUnsubscribed {
                shard_id: layer.meta.shard_id,
                participant_id: *self.id,
                stream_id: layer.stream_id(),
            },
        ));
    }

    pub fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket) {
        self.rtp_queue.push_back(RtpEvent { stream_id, pkt });
    }

    pub fn publish_track(&mut self, track: Track) {
        self.queue
            .push_back(ParticipantEvent::Control(ControlEvent::TrackPublished(
                track,
            )));
    }

    pub fn request_keyframe(&mut self, layer: &TrackLayer) {
        self.queue
            .push_back(ParticipantEvent::Control(ControlEvent::KeyframeRequested(
                GlobalKeyframeRequest {
                    shard_id: layer.meta.shard_id,
                    origin: layer.meta.origin,
                    stream_id: layer.stream_id(),
                    kind: KeyframeRequestKind::Pli,
                },
            )));
    }

    pub fn update_deadline(&mut self, deadline: Instant) {
        self.queue
            .push_back(ParticipantEvent::Timer(TimerEvent::DeadlineUpdated {
                at: deadline,
                participant_id: *self.id,
            }));
    }

    pub fn exit(&mut self) {
        self.queue
            .push_back(ParticipantEvent::Lifecycle(LifecycleEvent::Exited {
                participant_id: *self.id,
            }));
    }
}
