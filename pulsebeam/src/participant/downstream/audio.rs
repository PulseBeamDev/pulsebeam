use futures::stream::SelectAll;
use futures::stream::Stream;
use std::cmp::Ordering;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::media::Mid;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::monitor::StreamState;
use crate::rtp::timeline::Timeline;
use crate::track::TrackReceiver;

pub struct AudioAllocator {
    // Merges N streams into 1.
    // If all inputs are Pending (silence), this is Pending (0 CPU).
    inputs: SelectAll<IdentifyStream>,

    // Fixed output slots (e.g., 3)
    slots: Vec<AudioSlot>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            inputs: SelectAll::new(),
            slots: Vec::new(),
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        self.inputs.push(IdentifyStream {
            id: track.meta.id.clone(),
            inner: track,
        });
    }

    pub fn remove_track(&mut self, id: &Arc<TrackId>) {
        // SelectAll doesn't support random removal easily.
        // However, since TrackReceiver is a channel, dropping the Sender (upstream)
        // will close the Receiver, causing SelectAll to drop it automatically
        // on the next poll.
        //
        // If you need explicit removal, you have to rebuild the SelectAll
        // or use a wrapper that supports cancellation signals.
        //
        // For this implementation, we lazily clear the Slot ownership:
        for slot in &mut self.slots {
            if slot.current_track_id.as_ref() == Some(id) {
                slot.current_track_id = None;
            }
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.push(AudioSlot::new(mid));
    }

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        loop {
            // 1. Wait for ANY loud packet from ANY track
            let (track_id, packet) = match self.inputs.poll_next_unpin(cx) {
                Poll::Ready(Some(val)) => val,
                Poll::Ready(None) => return Poll::Ready(None), // All tracks gone
                Poll::Pending => return Poll::Pending,         // Sleep
            };

            let now = Instant::now();

            // 2. PRIORITY 1: Is this track already assigned?
            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.current_track_id.as_ref() == Some(&track_id))
            {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // 3. PRIORITY 2: Is there an empty slot?
            if let Some(slot) = self.slots.iter_mut().find(|s| s.current_track_id.is_none()) {
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // 4. PRIORITY 3: Can we evict someone?
            // Since slots are full, we check if anyone is stale.
            if let Some(slot) = self.slots.iter_mut().find(|s| s.can_evict(now)) {
                // Take the seat
                return Poll::Ready(Some(slot.process(&track_id, packet)));
            }

            // 5. PRIORITY 4: Full & Active
            // Everyone is talking. Drop the packet.
            // Continue loop to drain other pending packets if any.
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
            // Audio clock rate is typically 48000
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            current_track_id: None,
            last_active_at: Instant::now(),
        }
    }

    fn process(&mut self, track_id: &Arc<TrackId>, mut packet: RtpPacket) -> (Mid, RtpPacket) {
        let now = Instant::now();

        // Check if we are switching sources
        if self.current_track_id.as_ref() != Some(track_id) {
            // New speaker takes the chair
            self.current_track_id = Some(track_id.clone());

            // REBASE TIMELINE
            // We treat this packet as the "Keyframe" (sync point) for the switch.
            // We ensure the packet flag is set so your Timeline assertion passes.
            packet.is_keyframe_start = true;
            self.timeline.rebase(&packet);
        }

        self.last_active_at = now;

        let rewritten = self.timeline.rewrite(packet);
        (self.mid, rewritten)
    }

    /// Checks if the slot owner has timed out.
    fn can_evict(&self, now: Instant) -> bool {
        const HANGOVER: Duration = Duration::from_millis(350);
        match self.current_track_id {
            Some(_) => now.duration_since(self.last_active_at) > HANGOVER,
            None => true,
        }
    }
}

/// Wraps a TrackReceiver to attach the TrackId to every packet.
struct IdentifyStream {
    id: Arc<TrackId>,
    inner: TrackReceiver,
}

impl Stream for IdentifyStream {
    type Item = (Arc<TrackId>, RtpPacket);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(pkt)) => Poll::Ready(Some((self.id.clone(), pkt))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn compare_audio(a: &StreamState, b: &StreamState) -> Ordering {
    // Thresholds for stability
    const SPEECH_THRESHOLD: f32 = 0.01; // Minimum envelope to count as "talking"
    const ENVELOPE_EPSILON: f32 = 0.05; // 5% diff required to swap active speakers
    const SILENCE_EPSILON_MS: u128 = 500; // 500ms diff required to swap silent speakers

    let env_a = a.audio_envelope();
    let env_b = b.audio_envelope();
    let silence_a = a.silence_duration();
    let silence_b = b.silence_duration();

    let a_is_talking = env_a > SPEECH_THRESHOLD;
    let b_is_talking = env_b > SPEECH_THRESHOLD;

    // 1. BINARY PRIORITY: Active Speaker > Silent User
    if a_is_talking != b_is_talking {
        return if a_is_talking {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }

    // 2. MOMENTUM: Compare Intensity (if both active)
    if a_is_talking && b_is_talking {
        let diff = (env_a - env_b).abs();
        if diff > ENVELOPE_EPSILON {
            // Significant difference: Louder wins
            return env_b.partial_cmp(&env_a).unwrap();
        }
        // If difference is small (< 5%), treat as tied and fall through to Recency
    }

    // 3. RECENCY: Compare Silence Duration (Who spoke last?)
    let ms_a = silence_a.as_millis();
    let ms_b = silence_b.as_millis();
    let diff_ms = ms_a.abs_diff(ms_b);

    if diff_ms > SILENCE_EPSILON_MS {
        // Significant difference: More recent (lower duration) wins
        return ms_a.cmp(&ms_b);
    }

    // 4. TIE: Effectively identical audio state
    Ordering::Equal
}
