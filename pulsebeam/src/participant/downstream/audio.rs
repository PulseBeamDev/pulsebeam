use pulsebeam_runtime::sync::bit_signal::BitSignal;
use pulsebeam_runtime::sync::spmc;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::TrackReceiver;
use str0m::media::Mid;
use tokio::time::Instant;

// ---------------------------------------------------------------------------
// AudioAllocator
//
// Same single-task intra-concurrency model as VideoAllocator: poll_fn iterates
// all spmc receivers directly — no SelectAll, no unfold, no Arc/Mutex.
// ---------------------------------------------------------------------------

struct AudioInput {
    track_id: TrackId,
    receiver: spmc::Receiver<RtpPacket>,
}

pub struct AudioAllocator {
    inputs: Vec<AudioInput>,
    slots: Vec<AudioSlot>,
    last_polled: usize,
    shard_signal: Option<(Arc<BitSignal>, u64)>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self { inputs: Vec::new(), slots: Vec::new(), last_polled: 0, shard_signal: None }
    }

    /// Register a shard's BitSignal on all current and future audio inputs.
    pub fn attach_shard_signal(&mut self, signal: Arc<BitSignal>, bits: u64) {
        for input in &mut self.inputs {
            input.receiver.attach_signal(&signal, bits);
        }
        self.shard_signal = Some((signal, bits));
    }

    /// Non-blocking drain: returns the first ready audio packet without
    /// registering any wakers or allocating an EventListener.
    pub fn try_next(&mut self) -> Option<(Mid, RtpPacket)> {
        if self.inputs.is_empty() || self.slots.is_empty() {
            return None;
        }
        let len = self.inputs.len();
        for i in 0..len {
            let idx = (self.last_polled + i) % len;
            match self.inputs[idx].receiver.try_recv() {
                Some(Ok(packet)) => {
                    let track_id = self.inputs[idx].track_id;
                    self.last_polled = (idx + 1) % len;
                    return self.dispatch(track_id, packet);
                }
                Some(Err(spmc::RecvError::Lagged(n))) => {
                    tracing::warn!(track_id = %self.inputs[idx].track_id, skipped = n, "audio stream lagging");
                }
                Some(Err(spmc::RecvError::Closed)) => {
                    self.inputs.swap_remove(idx);
                    if self.last_polled >= self.inputs.len() {
                        self.last_polled = 0;
                    }
                    return None;
                }
                None => {}
            }
        }
        None
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        let id = track.meta.id;
        // Don't add duplicates.
        if self.inputs.iter().any(|i| i.track_id == id) {
            return;
        }
        let mut receiver = track.lowest_quality().channel.clone();
        if let Some((signal, bits)) = &self.shard_signal {
            receiver.attach_signal(signal, *bits);
        }
        self.inputs.push(AudioInput { track_id: id, receiver });
    }

    pub fn remove_track(&mut self, id: &TrackId) {
        // Called synchronously between poll_fn polls — safe to mutate directly.
        self.inputs.retain(|i| &i.track_id != id);
        for slot in &mut self.slots {
            if slot.current_track_id.as_ref() == Some(id) {
                slot.current_track_id = None;
            }
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.push(AudioSlot::new(mid));
    }

    fn dispatch(&mut self, track_id: TrackId, packet: RtpPacket) -> Option<(Mid, RtpPacket)> {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.as_ref() == Some(&track_id)) {
            return Some(slot.process(&track_id, packet));
        }
        if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.is_none()) {
            return Some(slot.process(&track_id, packet));
        }
        let now = Instant::now();
        if let Some(slot) = self.slots.iter_mut().find(|s| s.can_evict(now)) {
            return Some(slot.process(&track_id, packet));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// AudioSlot
// ---------------------------------------------------------------------------

struct AudioSlot {
    mid: Mid,
    timeline: Timeline,
    current_track_id: Option<TrackId>,
    last_active_at: Instant,
}

impl AudioSlot {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            current_track_id: None,
            last_active_at: Instant::now(),
        }
    }

    fn process(&mut self, track_id: &TrackId, packet: RtpPacket) -> (Mid, RtpPacket) {
        let now = Instant::now();

        if self.current_track_id.as_ref() != Some(track_id) {
            self.current_track_id = Some(*track_id);
            self.timeline.rebase(&packet);
        }

        self.last_active_at = now;
        (self.mid, self.timeline.rewrite(packet))
    }

    fn can_evict(&self, now: Instant) -> bool {
        const HANGOVER: Duration = Duration::from_millis(350);
        match self.current_track_id {
            Some(_) => now.duration_since(self.last_active_at) > HANGOVER,
            None => true,
        }
    }
}
