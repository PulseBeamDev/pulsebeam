use pulsebeam_runtime::sync::slot_group::SlotGroup;
use pulsebeam_runtime::sync::spmc;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::audio_selector::AudioSelectorSubscription;
use crate::controller::MAX_SEND_AUDIO_SLOTS;
use crate::entity::ParticipantId;
use crate::rtp;
use crate::rtp::timeline::Timeline;
use crate::rtp::{AudioRtpPacket, RtpPacket};
use str0m::media::Mid;
use str0m::rtp::Ssrc;

/// Per-participant audio allocator.
///
/// Call [`AudioAllocator::set_subscription`] once the room hands out the
/// [`AudioSelectorSubscription`], then call [`AudioAllocator::add_slot`] for
/// every audio `Mid` negotiated with the client.  The allocator pairs each
/// slot index with the matching subscription receiver index.
pub struct AudioAllocator {
    participant_id: ParticipantId,
    /// One stream per negotiated audio Mid.  Each stream wraps one selector
    /// output receiver + one `AudioSlot` (timeline rewriter).
    ///
    /// The `SlotGroup` gives O(woken) hot-path polling: it only visits slots
    /// that have been notified by the `spmc` ring's per-slot waker, using the
    /// same readiness-bitmask mechanism as `VideoAllocator`.
    slots: SlotGroup<AudioInputStream>,

    /// Stored subscription so that slots added *after* [`set_subscription`]
    /// is called receive their receiver immediately in [`add_slot`].
    ///
    /// Without this, the subscription arrives from the room when the
    /// participant first joins (before any SDP negotiation), while
    /// [`add_slot`] is called later as each audio `Mid` is negotiated.
    /// Keeping the subscription here eliminates the ordering dependency.
    pending_sub: Option<AudioSelectorSubscription>,
}

impl AudioAllocator {
    pub fn new(participant_id: ParticipantId) -> Self {
        Self {
            participant_id,
            slots: SlotGroup::with_capacity(MAX_SEND_AUDIO_SLOTS),
            pending_sub: None,
        }
    }

    /// Replace every slot's receiver with the corresponding one from a
    /// (new) selector subscription, and store the subscription so that
    /// slots added later via [`add_slot`] are also wired up immediately.
    ///
    /// Pokes each slot after swapping the receiver so the `SlotGroup` wakes
    /// it on the next poll — necessary because the previous receiver may have
    /// been closed/pending and the new ring head is unknown to the bitmask.
    pub fn set_subscription(&mut self, sub: AudioSelectorSubscription) {
        for (i, receiver) in sub.receivers.iter().enumerate() {
            if let Some(mut stream) = self.slots.get_mut(i) {
                stream.set_receiver(receiver.clone());
            }
        }
        self.pending_sub = Some(sub);
    }

    /// Pin a specific slot to a direct `spmc::Receiver`, bypassing the room
    /// selector for that slot.
    ///
    /// Useful when a participant wants to always show a particular speaker
    /// regardless of the Top-N ranking.  Returns `false` if `slot_index` has
    /// not yet been registered via [`add_slot`].
    #[allow(unused)]
    pub fn pin_slot(
        &mut self,
        slot_index: usize,
        receiver: spmc::Receiver<AudioRtpPacket>,
    ) -> bool {
        let Some(mut stream) = self.slots.get_mut(slot_index) else {
            return false;
        };
        stream.set_receiver(receiver);
        true
    }

    /// Remove a slot pin, restoring the room-level selector receiver for that
    /// slot.  Requires the original subscription to restore from.
    #[allow(unused)]
    pub fn unpin_slot(&mut self, slot_index: usize, sub: &AudioSelectorSubscription) -> bool {
        if slot_index >= sub.receivers.len() {
            return false;
        }
        let Some(mut stream) = self.slots.get_mut(slot_index) else {
            return false;
        };
        stream.set_receiver(sub.receivers[slot_index].clone());
        true
    }

    /// Register a negotiated audio `Mid`.
    ///
    /// Slots are paired 1-to-1 with subscription receivers by insertion order
    /// (slot 0 ↔ receiver 0, slot 1 ↔ receiver 1, …).  If a subscription
    /// has already been delivered via [`set_subscription`], the matching
    /// receiver is wired in immediately so packets flow without waiting for
    /// a second subscription message.
    pub fn add_slot(&mut self, mid: Mid) {
        let idx = self.slots.insert(AudioInputStream::new(mid));
        if let Some(sub) = &self.pending_sub
            && let Some(receiver) = sub.receivers.get(idx)
            && let Some(mut stream) = self.slots.get_mut(idx)
        {
            stream.set_receiver(receiver.clone());
        }
    }

    /// Inline hot path — called directly from `DownstreamAllocator::poll_next`.
    ///
    /// Delegates entirely to `SlotGroup::poll_next`, which uses an atomic
    /// readiness bitmask to visit only slots that have pending data.
    #[inline]
    pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        use futures_lite::stream::Stream as _;

        // Drop packets that originate from this participant to avoid sending
        // loopback audio back to the sender.
        loop {
            match Pin::new(&mut self.slots).poll_next(cx) {
                Poll::Ready(Some((mid, packet))) => {
                    if packet.participant_id == self.participant_id {
                        // Keep draining until we find a packet from another participant.
                        continue;
                    }
                    return Poll::Ready(Some((mid, packet.packet)));
                }
                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ── Inner stream ─────────────────────────────────────────────────────────────

/// One audio stream stored inside the `SlotGroup`.
///
/// Wraps an optional `spmc::Receiver` (absent until [`set_subscription`] runs)
/// and an `AudioSlot` that rewrites timestamps.
///
/// Implements `Stream<Item = (Mid, RtpPacket)>` so it can live in a
/// `SlotGroup`.  Never returns `Poll::Ready(None)` — slots are permanent for
/// the session lifetime; only the underlying receiver is swapped on
/// subscription/pin changes.
struct AudioInputStream {
    receiver: Option<spmc::Receiver<AudioRtpPacket>>,
    slot: AudioSlot,
}

impl AudioInputStream {
    fn new(mid: Mid) -> Self {
        Self {
            receiver: None,
            slot: AudioSlot::new(mid),
        }
    }

    /// Swap in a new receiver and reset the timeline so the SSRC-change
    /// detection in `AudioSlot::process` triggers a clean rebase on the
    /// first packet from the new source.
    fn set_receiver(&mut self, receiver: spmc::Receiver<AudioRtpPacket>) {
        self.receiver = Some(receiver);
        self.slot.last_ssrc = None;
    }
}

impl futures_lite::stream::Stream for AudioInputStream {
    type Item = (Mid, AudioRtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(receiver) = this.receiver.as_mut() else {
            // No subscription yet — stay pending until set_receiver + poke.
            return Poll::Pending;
        };
        match receiver.poll_recv(cx) {
            Poll::Ready(Ok(packet)) => Poll::Ready(Some(this.slot.process(packet))),
            Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                tracing::warn!(mid = ?this.slot.mid, skipped = n, "audio slot lagging");
                // The ring head has advanced past the missed packets.  Re-arm
                // this slot's waker (cx here is the per-slot SlotGroup waker)
                // so we are visited again on the next outer poll_next call.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(spmc::RecvError::Closed)) => {
                // The selector task shut down — should not happen in normal
                // operation.  Stay pending; set_receiver + poke will revive
                // this slot if the room restarts the selector.
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ── Slot ─────────────────────────────────────────────────────────────────────

/// Timeline-rewriting output slot for one audio `Mid`.
struct AudioSlot {
    mid: Mid,
    timeline: Timeline,
    /// Last seen SSRC.  When it changes (selector assigned a different speaker)
    /// we rebase the `Timeline` for a glitch-free transition at the client.
    last_ssrc: Option<Ssrc>,
}

impl AudioSlot {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            last_ssrc: None,
        }
    }

    fn process(&mut self, mut p: AudioRtpPacket) -> (Mid, AudioRtpPacket) {
        if self.last_ssrc != Some(p.packet.ssrc) {
            self.last_ssrc = Some(p.packet.ssrc);
            self.timeline.rebase(&p.packet);
        }
        p.packet = self.timeline.rewrite(p.packet);
        (self.mid, p)
    }
}
