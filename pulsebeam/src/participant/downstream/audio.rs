use futures_lite::StreamExt;
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::task::{Context, Poll, Waker};

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::TrackReceiver;
use std::time::Duration;
use str0m::media::Mid;
use tokio::time::Instant;

pub struct AudioAllocator {
    inputs: HashMap<TrackId, spmc::Receiver<RtpPacket>>,
    slots: Vec<AudioSlot>,
    waker: Option<Waker>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            slots: Vec::new(),
            waker: None,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        self.inputs
            .insert(track.meta.id, track.lowest_quality().channel.clone());
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn remove_track(&mut self, id: &TrackId) {
        self.inputs.remove(id);

        for slot in &mut self.slots {
            if slot.current_track_id.as_ref() == Some(id) {
                slot.current_track_id = None;
            }
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.push(AudioSlot::new(mid));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        loop {
            if self.inputs.is_empty() || self.slots.is_empty() {
                self.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }

            let mut found_packet = None;
            for (track_id, receiver) in &mut self.inputs {
                match receiver.poll_next(cx) {
                    Poll::Ready(Some(Ok(packet))) => {
                        found_packet = Some((*track_id, packet));
                        break; // Process one packet at a time
                    }
                    Poll::Ready(Some(Err(err))) => {
                        tracing::warn!("{} stream is lagging: {:?}", track_id, err);
                        continue;
                    }
                    Poll::Ready(None) | Poll::Pending => {
                        continue;
                    }
                }
            }

            let (track_id, packet) = match found_packet {
                Some(p) => p,
                None => return Poll::Pending,
            };

            let now = Instant::now();

            // Try to find existing slot for this track
            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.current_track_id.as_ref() == Some(&track_id))
            {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // Try to find an empty slot
            if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.is_none()) {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // Try to find an evictable slot
            if let Some(slot) = self.slots.iter_mut().find(|s| s.can_evict(now)) {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // No slot available, drop and continue
            continue;
        }
    }
}

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

        let rewritten = self.timeline.rewrite(packet);
        (self.mid, rewritten)
    }

    fn can_evict(&self, now: Instant) -> bool {
        const HANGOVER: Duration = Duration::from_millis(350);
        match self.current_track_id {
            Some(_) => now.duration_since(self.last_active_at) > HANGOVER,
            None => true,
        }
    }
}
