use std::array;

use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

use crate::audio_selector::SELECTOR_SLOTS;
use crate::rtp::RtpPacket;
use crate::track::StreamWriter;

/// Downstream audio allocator.
///
/// Holds the fixed mapping of slot index → (Mid, Pt, Ssrc) for this subscriber.
/// Timeline rewriting and top-N selection are owned by the shard-level
/// [`TopNAudioSelector`]; this type only applies the N→M filter (M ≤ N slots
/// are provisioned for this subscriber).
pub struct AudioAllocator {
    /// M ≤ N provisioned slots; `None` entries are unfilled.
    slots: [Option<Slot>; SELECTOR_SLOTS],
}

pub struct Slot {
    pt: Pt,
    mid: Mid,
    ssrc: Ssrc,
    pending_marker: bool,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            slots: array::from_fn(|_| None),
        }
    }

    pub fn add_slot(&mut self, mid: Mid, pt: Pt, ssrc: Ssrc) {
        if let Some(entry) = self.slots.iter_mut().find(|s| s.is_none()) {
            *entry = Some(Slot {
                mid,
                pt,
                ssrc,
                pending_marker: true,
            });
        }
    }

    pub fn on_rtp(
        &mut self,
        slot_idx: usize,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> Option<()> {
        let slot = self.slots.get_mut(slot_idx)?.as_mut()?;
        let mut pkt = pkt.clone();
        if slot.pending_marker {
            pkt.marker = true;
            slot.pending_marker = false;
        }
        writer.write_owned(pkt, &slot.ssrc, slot.pt);
        Some(())
    }
}
