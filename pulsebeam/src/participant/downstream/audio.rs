use futures::StreamExt;
use futures::stream::SelectAll;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Waker;
use std::task::ready;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::media::Mid;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::SimulcastReceiver;
use crate::track::TrackReceiver;
use pulsebeam_runtime::sync::spmc::RecvError;

pub struct AudioAllocator {
    inputs: SelectAll<IdentifyStream>,
    slots: Vec<AudioSlot>,
    waker: Option<Waker>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            inputs: SelectAll::new(),
            slots: Vec::new(),
            waker: None,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        self.inputs.push(IdentifyStream {
            id: track.meta.id.clone(),
            inner: track.lowest_quality().clone(),
        });
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn remove_track(&mut self, id: &Arc<TrackId>) {
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
        // SelectAll will return Ready(None) with an empty list
        if self.inputs.is_empty() || self.slots.is_empty() {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let mut budget = 10;
        loop {
            if budget == 0 {
                // Force a wake so we return immediately after the runtime
                // polls other tasks (like Video).
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            budget -= 1;

            let (track_id, packet) = match self.inputs.poll_next_unpin(cx) {
                Poll::Ready(Some(val)) => val,
                Poll::Ready(None) => return Poll::Ready(None), // All inputs closed
                Poll::Pending => return Poll::Pending,         // No audio data
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
    current_track_id: Option<Arc<TrackId>>,
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

    fn process(&mut self, track_id: &Arc<TrackId>, packet: RtpPacket) -> (Mid, RtpPacket) {
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

struct IdentifyStream {
    id: Arc<TrackId>,
    inner: SimulcastReceiver,
}

impl Stream for IdentifyStream {
    type Item = (Arc<TrackId>, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match ready!(this.inner.channel.poll_recv(cx)) {
                Ok(pkt) => return Poll::Ready(Some((this.id.clone(), pkt))),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(track_id = %this.id, "audio stream is lagging by {}", n);
                    // will continue to read from the latest
                }
                Err(RecvError::Closed) => return Poll::Ready(None),
            };
        }
    }
}
