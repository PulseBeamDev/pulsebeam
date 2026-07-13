use std::collections::VecDeque;

use crate::participant::event::{ParticipantEvent, RtpEvent};

use super::worker::ShardEvent;

pub(crate) struct EventPipeline {
    participant_events: VecDeque<ParticipantEvent>,
    rtp_events: VecDeque<RtpEvent>,
    shard_events: VecDeque<ShardEvent>,
}

impl EventPipeline {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            participant_events: VecDeque::with_capacity(cap),
            rtp_events: VecDeque::with_capacity(cap),
            shard_events: VecDeque::with_capacity(cap),
        }
    }

    /// The two sinks `EventQueue` (participant.rs) writes into during `poll`.
    pub fn poll_sinks(&mut self) -> (&mut VecDeque<ParticipantEvent>, &mut VecDeque<RtpEvent>) {
        (&mut self.participant_events, &mut self.rtp_events)
    }

    pub fn pop_participant_event(&mut self) -> Option<ParticipantEvent> {
        self.participant_events.pop_front()
    }

    pub fn pop_rtp_event(&mut self) -> Option<RtpEvent> {
        self.rtp_events.pop_front()
    }

    pub fn push_shard_event(&mut self, ev: ShardEvent) {
        self.shard_events.push_back(ev);
    }

    pub fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.shard_events.pop_front()
    }

    pub fn shard_events_mut(&mut self) -> &mut VecDeque<ShardEvent> {
        &mut self.shard_events
    }
}
