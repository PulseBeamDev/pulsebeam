use pulsebeam_runtime::sync::spmc;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::TrackReceiver;
use str0m::media::Mid;
use tokio::time::Instant;

struct AudioInput {
    track_id: TrackId,
    receiver: spmc::Receiver<RtpPacket>,
}

pub struct AudioAllocator {
    inputs: Vec<AudioInput>,
    slots: Vec<AudioSlot>,
    last_polled: usize,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            slots: Vec::new(),
            last_polled: 0,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        let id = track.meta.id;
        // Don't add duplicates.
        if self.inputs.iter().any(|i| i.track_id == id) {
            return;
        }
        let receiver = track.lowest_quality().channel.clone();
        self.inputs.push(AudioInput {
            track_id: id,
            receiver,
        });
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

    /// Await the next outbound audio packet.
    ///
    /// Hot path: calls `poll_recv` on each input receiver directly in a
    /// `poll_fn` loop — zero heap allocation, zero `event_listener`
    /// registrations when data is already in the ring.
    pub async fn next(&mut self) -> (Mid, RtpPacket) {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<(Mid, RtpPacket)> {
        if self.inputs.is_empty() || self.slots.is_empty() {
            return Poll::Pending;
        }

        let now = Instant::now();
        for i in 0..self.inputs.len() {
            let input = &mut self.inputs[i];
            match input.receiver.poll_recv(cx) {
                Poll::Ready(Ok(packet)) => {
                    let track_id = input.track_id;
                    if let Some(result) = self.dispatch(track_id, packet, now) {
                        return Poll::Ready(result);
                    } else {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                    tracing::warn!(track_id = %input.track_id, skipped = n, "audio stream lagging");
                }
                Poll::Ready(Err(spmc::RecvError::Closed)) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Pending => {}
            }
        }

        Poll::Pending
    }

    fn dispatch(
        &mut self,
        track_id: TrackId,
        packet: RtpPacket,
        now: Instant,
    ) -> Option<(Mid, RtpPacket)> {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|s| s.current_track_id.as_ref() == Some(&track_id))
        {
            return Some(slot.process(&track_id, packet));
        }
        if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.is_none()) {
            return Some(slot.process(&track_id, packet));
        }
        if let Some(slot) = self.slots.iter_mut().find(|s| s.can_evict(now)) {
            return Some(slot.process(&track_id, packet));
        }
        None
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
