use std::array;

use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

use crate::audio_selector::SELECTOR_SLOTS;
use crate::id::AudioSelectorSlotId;
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
        if self.has_slot(slot.mid) {
            tracing::debug!(
                target: crate::log::TARGET_AUDIO,
                mid = %slot.mid,
                "audio slot already provisioned; skipping duplicate"
            );
            return;
        }
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

    pub fn has_slot(&self, mid: Mid) -> bool {
        self.slots.iter().flatten().any(|slot| slot.mid == mid)
    }

    pub fn on_rtp(
        &mut self,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> Option<()> {
        let Some(slot_entry) = self.slots.get_mut(slot_idx.index()) else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                slot_idx = %slot_idx,
                slots = self.slots.len(),
                "audio allocator received out-of-range slot index"
            );
            return None;
        };
        let Some(slot) = slot_entry.as_mut() else {
            tracing::debug!(
                target: crate::log::TARGET_AUDIO,
                slot_idx = %slot_idx,
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
        writer.write_audio_owned(pkt, slot.mid, slot.pt);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant::downstream::SlotConfig;
    use crate::rtp::RtpPacket;
    use crate::track::StreamWriter;
    use str0m::RtcConfig;
    use str0m::media::{MediaKind, Mid, Pt};
    use str0m::rtp::Ssrc;

    fn make_audio_slot() -> SlotConfig {
        SlotConfig {
            mid: Mid::from("a0"),
            rid: None,
            ssrc: Ssrc::from(1234_u32),
            pt: Pt::from(111_u8),
            kind: MediaKind::Audio,
        }
    }

    #[test]
    fn first_forwarded_packet_clears_pending_marker() {
        let mut alloc = AudioAllocator::new();
        alloc.add_slot(make_audio_slot());
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "new audio slot must start with pending marker"
        );

        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut writer = StreamWriter(&mut rtc);

        let first = RtpPacket::default();
        let _ = alloc.on_rtp(AudioSelectorSlotId::new(0), &first, &mut writer);
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "first forwarded packet must consume pending marker"
        );

        let second = RtpPacket::default();
        let _ = alloc.on_rtp(AudioSelectorSlotId::new(0), &second, &mut writer);
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "pending marker must stay cleared for subsequent packets"
        );
    }

    #[test]
    fn unprovisioned_slot_does_not_toggle_other_slots() {
        let mut alloc = AudioAllocator::new();
        alloc.add_slot(make_audio_slot());

        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut writer = StreamWriter(&mut rtc);

        let pkt = RtpPacket::default();
        let res = alloc.on_rtp(AudioSelectorSlotId::new(1), &pkt, &mut writer);
        assert!(res.is_none(), "unprovisioned slot must be dropped");
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "dropping another slot must not consume pending marker"
        );
    }

    #[test]
    fn duplicate_mid_is_ignored() {
        let mut alloc = AudioAllocator::new();
        let slot = make_audio_slot();
        alloc.add_slot(slot.clone());
        alloc.add_slot(slot);

        let provisioned = alloc.slots.iter().flatten().count();
        assert_eq!(
            provisioned, 1,
            "duplicate mid must not consume a second slot"
        );
    }
}
