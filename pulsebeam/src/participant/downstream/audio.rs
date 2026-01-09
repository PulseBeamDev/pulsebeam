use futures_lite::StreamExt;
use pulsebeam_runtime::sync::spmc;

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::TrackReceiver;
use std::sync::Arc;
use std::task::Waker;
use std::task::ready;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::media::Mid;
use tokio::time::Instant;
use tokio_stream::StreamMap;

pub struct AudioAllocator {
    inputs: StreamMap<TrackId, spmc::Receiver<RtpPacket>>,
    slots: Vec<AudioSlot>,
    waker: Option<Waker>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            inputs: StreamMap::new(),
            slots: Vec::new(),
            waker: None,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        self.inputs.insert(
            track.meta.id.clone(),
            track.lowest_quality().channel.clone(),
        );
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn remove_track(&mut self, id: &TrackId) {
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
        // TODO: use assignment then poll just the assignment

        loop {
            // SelectAll will return Ready(None) with an empty list
            if self.inputs.is_empty() || self.slots.is_empty() {
                self.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }

            let (track_id, packet) = match ready!(self.inputs.poll_next(cx)) {
                Some((track_id, Ok(packet))) => (track_id, packet),
                Some((track_id, Err(err))) => {
                    tracing::warn!("{} stream is lagging: {:?}", track_id, err);
                    continue;
                }
                None => return Poll::Ready(None), // All inputs closed
            };

            let now = Instant::now();

            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.current_track_id.as_ref() == Some(&track_id))
            {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.is_none()) {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            if let Some(slot) = self.slots.iter_mut().find(|s| s.can_evict(now)) {
                // Take the seat
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }
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
            self.current_track_id = Some(track_id.clone());
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
