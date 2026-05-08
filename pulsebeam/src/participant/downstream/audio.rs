use std::array;

use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

use crate::audio_selector::SELECTOR_SLOTS;
use crate::participant::downstream::SlotConfig;
use crate::rtp::RtpPacket;
use crate::track::StreamWriter;

/// Downstream audio allocator.
///
/// Holds the fixed mapping of slot index → (Mid, Pt, Ssrc) for this subscriber.
/// Timeline rewriting and marker-on-switch are handled upstream by the shard-level
/// [`TopNAudioSelector`]; packets arriving here are already continuous.
pub struct AudioAllocator {
    /// M ≤ N provisioned slots; `None` entries are unfilled.
    slots: [Option<Slot>; SELECTOR_SLOTS],
}

pub struct Slot {
    pt: Pt,
    mid: Mid,
    ssrc: Ssrc,
    /// Set to `true` when the slot is first provisioned for this subscriber so the
    /// very first forwarded packet carries the RTP marker bit (talk-spurt start).
    pending_marker: bool,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            slots: array::from_fn(|_| None),
        }
    }

    pub fn add_slot(&mut self, slot: SlotConfig) {
        if let Some(entry) = self.slots.iter_mut().find(|s| s.is_none()) {
            *entry = Some(Slot {
                mid: slot.mid,
                pt: slot.pt,
                ssrc: slot.ssrc,
                pending_marker: true,
            });
        } else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                mid = %slot.mid,
                pt = %slot.pt,
                ssrc = %slot.ssrc,
                slots = SELECTOR_SLOTS,
                "audio allocator has no free slot; dropping slot provisioning"
            );
        }
    }

    pub fn on_rtp(
        &mut self,
        slot_idx: usize,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> Option<()> {
        let Some(slot_entry) = self.slots.get_mut(slot_idx) else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                slot_idx,
                slots = self.slots.len(),
                "audio allocator received out-of-range slot index"
            );
            return None;
        };
        let Some(slot) = slot_entry.as_mut() else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                slot_idx,
                slots = self.slots.len(),
                "audio allocator received packet for unprovisioned slot"
            );
            return None;
        };
        let mut pkt = pkt.clone();
        if slot.pending_marker {
            pkt.marker = true;
            slot.pending_marker = false;
        }
        writer.write_audio_owned(pkt, &slot.ssrc, slot.pt);
        Some(())
    }
}
