use std::collections::VecDeque;

use str0m::media::KeyframeRequestKind;
use tokio::time::Instant;

use crate::entity::ParticipantId;
use crate::rtp::RtpPacket;
use crate::track::{GlobalKeyframeRequest, StreamId, Track, TrackLayer};

pub enum ParticipantEvent {
    Media(MediaEvent),
    Topology(TopologyEvent),
    Timer(TimerEvent),
    Lifecycle(LifecycleEvent),
    Control(ControlEvent),
}

pub enum MediaEvent {
    RtpPublished { stream_id: StreamId, pkt: RtpPacket },
}

pub enum TopologyEvent {
    StreamSubscribed {
        participant_id: ParticipantId,
        stream_id: StreamId,
    },
    StreamUnsubscribed {
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
}

impl<'a> EventQueue<'a> {
    pub fn new(id: &'a ParticipantId, queue: &'a mut VecDeque<ParticipantEvent>) -> Self {
        Self { id, queue }
    }

    pub fn subscribe(&mut self, stream_id: StreamId) {
        self.queue.push_back(ParticipantEvent::Topology(
            TopologyEvent::StreamSubscribed {
                participant_id: *self.id,
                stream_id,
            },
        ));
    }

    pub fn unsubscribe(&mut self, stream_id: StreamId) {
        self.queue.push_back(ParticipantEvent::Topology(
            TopologyEvent::StreamUnsubscribed {
                participant_id: *self.id,
                stream_id,
            },
        ));
    }

    pub fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket) {
        self.queue
            .push_back(ParticipantEvent::Media(MediaEvent::RtpPublished {
                stream_id,
                pkt,
            }));
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
