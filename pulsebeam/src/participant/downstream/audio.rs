use std::array;

use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

use crate::audio_selector::SELECTOR_SLOTS;
use crate::entity::{AudioOrigin, TrackId};
use crate::id::AudioSelectorSlotId;
use crate::log::{LogCtx, plog_debug, plog_warn};
use crate::participant::downstream::SlotConfig;
use crate::rtp::RtpPacket;
use crate::track::StreamWriter;

/// Downstream audio allocator.
///
/// Holds the fixed mapping of slot index → (Mid, Pt, Ssrc) for this subscriber.
/// Timeline rewriting and marker-on-switch are handled upstream by the shard-level
/// [`TopNAudioSelector`]; packets arriving here are already continuous.
pub struct AudioAllocator {
    ctx: LogCtx,
    /// M ≤ N provisioned slots; `None` entries are unfilled.
    slots: [Option<Slot>; SELECTOR_SLOTS],
    speakers_changed: bool,
}

pub struct Slot {
    pt: Pt,
    mid: Mid,
    ssrc: Ssrc,
    /// Set to `true` when the slot is first provisioned for this subscriber so the
    /// very first forwarded packet carries the RTP marker bit (talk-spurt start).
    pending_marker: bool,
    /// Who this slot is currently carrying, and how loud they were.
    ///
    /// The selector steals slots as people start and stop talking, so a subscriber cannot work
    /// out who it is hearing from the media: every slot is one continuous stream on one mid
    /// whoever is speaking. Recording it here is what lets the assignment be signalled.
    occupant: Option<Occupant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Occupant {
    origin: AudioOrigin,
    /// Most recent loudness, RFC 6464 negative dBov.
    level_dbov: i8,
}

/// A speaker this subscriber is currently hearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard {
    pub mid: Mid,
    pub origin: AudioOrigin,
    pub level_dbov: i8,
}

impl AudioAllocator {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            slots: array::from_fn(|_| None),
            speakers_changed: false,
        }
    }

    pub fn add_slot(&mut self, slot: SlotConfig) {
        if self.has_slot(slot.mid) {
            plog_debug!(
                self.ctx,
                target: crate::log::TARGET_AUDIO,
                mid = %slot.mid,
                "audio slot already provisioned; skipping duplicate"
            );
            return;
        }
        for entry in &mut self.slots {
            if entry.is_none() {
                *entry = Some(Slot {
                    mid: slot.mid,
                    pt: slot.pt,
                    ssrc: slot.ssrc,
                    pending_marker: true,
                    occupant: None,
                });
                return;
            }
        }
        plog_warn!(
            self.ctx,
            target: crate::log::TARGET_AUDIO,
            mid = %slot.mid,
            pt = %slot.pt,
            ssrc = %slot.ssrc,
            slots = SELECTOR_SLOTS,
            "audio allocator has no free slot; dropping slot provisioning"
        );
    }

    /// Forget a speaker who has left, so nothing goes on announcing them.
    ///
    /// A slot holds its last occupant until somebody takes it, which is right while the speaker is
    /// merely quiet and wrong once they have gone: the assignment naming them keeps being sent,
    /// the client keeps a publication, and the room shows a tile for somebody who is not in it.
    /// Returns whether this slot was carrying them.
    pub fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let mut removed = false;
        for slot in self.slots.iter_mut().flatten() {
            if slot.occupant.is_some_and(|o| o.origin.track == *track_id) {
                slot.occupant = None;
                // The next speaker to take this slot starts a talk spurt, whoever they are.
                slot.pending_marker = true;
                removed = true;
            }
        }
        removed
    }

    pub fn has_slot(&self, mid: Mid) -> bool {
        self.slots.iter().flatten().any(|slot| slot.mid == mid)
    }

    pub fn refresh_ssrc(&mut self, mid: Mid, ssrc: Ssrc) -> bool {
        for slot_entry in &mut self.slots {
            if let Some(slot) = slot_entry.as_mut()
                && slot.mid == mid
            {
                slot.ssrc = ssrc;
                return true;
            }
        }
        false
    }

    /// Whether someone new took over a slot since this was last asked.
    pub fn take_speakers_changed(&mut self) -> bool {
        std::mem::take(&mut self.speakers_changed)
    }

    /// Who this subscriber is hearing, loudest first.
    ///
    /// Ranked on the level of the audio actually delivered rather than on the selector's internal
    /// decay: this is what the subscriber can hear, which is what a UI highlights and a recording
    /// logs.
    pub fn assignments(&self) -> Vec<Heard> {
        let mut heard: Vec<Heard> = self
            .slots
            .iter()
            .flatten()
            .filter_map(|slot| {
                slot.occupant.map(|occupant| Heard {
                    mid: slot.mid,
                    origin: occupant.origin,
                    level_dbov: occupant.level_dbov,
                })
            })
            .collect();
        // Descending loudness, so rank 0 is the loudest. `level_dbov` is negative, so a larger
        // value is louder.
        heard.sort_by(|a, b| {
            b.level_dbov
                .cmp(&a.level_dbov)
                .then_with(|| a.mid.as_bytes().cmp(b.mid.as_bytes()))
        });
        heard
    }

    pub fn on_rtp(
        &mut self,
        slot_idx: AudioSelectorSlotId,
        origin: AudioOrigin,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> Option<()> {
        let Some(slot_entry) = self.slots.get_mut(slot_idx.index()) else {
            plog_warn!(
                self.ctx,
                target: crate::log::TARGET_AUDIO,
                slot_idx = %slot_idx,
                slots = self.slots.len(),
                "audio allocator received out-of-range slot index"
            );
            return None;
        };
        let Some(slot) = slot_entry.as_mut() else {
            plog_debug!(
                self.ctx,
                target: crate::log::TARGET_AUDIO,
                slot_idx = %slot_idx,
                slots = self.slots.len(),
                "audio allocator received packet for unprovisioned slot"
            );
            return None;
        };
        let level_dbov = pkt.ext_vals.audio_level.unwrap_or(i8::MIN);
        let next = Occupant { origin, level_dbov };
        // A change of owner is a change of speaker, which the subscriber has to be told about:
        // the mid and the SSRC stay put across a steal, so nothing else distinguishes the new
        // voice from the old one. Loudness moves with every packet and is deliberately not a
        // trigger - it is carried on the next update the switch causes, so a room where two
        // people are talking at similar volume does not produce a signalling message per packet.
        let switched = slot.occupant.map(|o| o.origin) != Some(next.origin);
        self.speakers_changed |= switched;
        slot.occupant = Some(next);
        let mut pkt = pkt.clone();
        if slot.pending_marker {
            pkt.marker = true;
            slot.pending_marker = false;
        }
        writer.write_audio_owned(pkt, slot.mid, slot.ssrc, slot.pt);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::participant::downstream::SlotConfig;
    use crate::rtp::RtpPacket;
    use str0m::media::{MediaKind, Mid, Pt};
    use str0m::rtp::Ssrc;

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, ParticipantId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(1)),
        }
    }

    fn origin(seed: u64) -> AudioOrigin {
        use crate::entity::{ParticipantId, TrackKind};
        let mut rng = pulsebeam_runtime::rand::seeded_rng(seed);
        let participant = ParticipantId::new(&mut rng);
        AudioOrigin {
            track: participant.derive_track_id(TrackKind::Audio, "mic"),
            participant,
        }
    }

    fn speaking(level_dbov: i8) -> RtpPacket {
        let mut pkt = RtpPacket::default();
        pkt.ext_vals.audio_level = Some(level_dbov);
        pkt
    }

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
        let mut alloc = AudioAllocator::new(test_ctx());
        alloc.add_slot(make_audio_slot());
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "new audio slot must start with pending marker"
        );

        let first = RtpPacket::default();
        let _ = alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            origin(1),
            &first,
            &mut StreamWriter::new(),
        );
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "first forwarded packet must consume pending marker"
        );

        let second = RtpPacket::default();
        let _ = alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            origin(1),
            &second,
            &mut StreamWriter::new(),
        );
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "pending marker must stay cleared for subsequent packets"
        );
    }

    #[test]
    fn unprovisioned_slot_does_not_toggle_other_slots() {
        let mut alloc = AudioAllocator::new(test_ctx());
        alloc.add_slot(make_audio_slot());

        let pkt = RtpPacket::default();
        let res = alloc.on_rtp(
            AudioSelectorSlotId::new(1),
            origin(1),
            &pkt,
            &mut StreamWriter::new(),
        );
        assert!(res.is_none(), "unprovisioned slot must be dropped");
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "dropping another slot must not consume pending marker"
        );
    }

    fn two_slots() -> [SlotConfig; 2] {
        [
            SlotConfig {
                mid: Mid::from("a0"),
                rid: None,
                ssrc: Ssrc::from(1234_u32),
                pt: Pt::from(111_u8),
                kind: MediaKind::Audio,
            },
            SlotConfig {
                mid: Mid::from("a1"),
                rid: None,
                ssrc: Ssrc::from(1235_u32),
                pt: Pt::from(111_u8),
                kind: MediaKind::Audio,
            },
        ]
    }

    /// The subscriber cannot tell who it is hearing from the media, so the allocator has to
    /// remember. Loudest first, because that is the order a UI draws.
    #[test]
    fn who_is_heard_is_ranked_by_loudness() {
        let mut alloc = AudioAllocator::new(test_ctx());
        for slot in two_slots() {
            alloc.add_slot(slot);
        }
        let quiet = origin(1);
        let loud = origin(2);

        let mut writer = StreamWriter::new();
        alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            quiet,
            &speaking(-40),
            &mut writer,
        );
        alloc.on_rtp(
            AudioSelectorSlotId::new(1),
            loud,
            &speaking(-15),
            &mut writer,
        );

        let heard = alloc.assignments();
        assert_eq!(heard.len(), 2, "both slots are occupied");
        assert_eq!(heard[0].origin, loud, "the loudest speaker ranks first");
        assert_eq!(heard[1].origin, quiet);
        assert_eq!(heard[0].mid, Mid::from("a1"));
    }

    /// Only a change of occupant is worth a signalling message. Loudness moves with every packet,
    /// so making it a trigger would put one message on the wire per packet.
    #[test]
    fn only_a_new_occupant_counts_as_a_change() {
        let mut alloc = AudioAllocator::new(test_ctx());
        alloc.add_slot(make_audio_slot());
        let first = origin(1);
        let second = origin(2);
        let mut writer = StreamWriter::new();

        alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            first,
            &speaking(-30),
            &mut writer,
        );
        assert!(alloc.take_speakers_changed(), "a slot was filled");
        assert!(
            !alloc.take_speakers_changed(),
            "asking twice does not repeat it"
        );

        alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            first,
            &speaking(-9),
            &mut writer,
        );
        assert!(
            !alloc.take_speakers_changed(),
            "the same speaker getting louder is not a change of speaker"
        );

        alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            second,
            &speaking(-9),
            &mut writer,
        );
        assert!(
            alloc.take_speakers_changed(),
            "the slot was stolen, and nothing else on the wire says so"
        );
        assert_eq!(alloc.assignments()[0].origin, second);
    }

    /// A speaker who leaves stops occupying their slot.
    ///
    /// A slot holds its last occupant until somebody takes it, which is right while they are
    /// merely quiet and wrong once they have gone: the assignment naming them keeps being sent and
    /// the client keeps showing them. Nothing cleared it - `remove_track` only ever reached the
    /// video allocator.
    #[test]
    fn a_departed_speaker_stops_occupying_their_slot() {
        let mut alloc = AudioAllocator::new(test_ctx());
        alloc.add_slot(make_audio_slot());
        let speaker = origin(1);
        let mut writer = StreamWriter::new();
        alloc.on_rtp(
            AudioSelectorSlotId::new(0),
            speaker,
            &speaking(-30),
            &mut writer,
        );
        assert_eq!(alloc.assignments().len(), 1, "they are being heard");

        assert!(
            alloc.remove_track(&speaker.track),
            "the slot was carrying them"
        );
        assert!(
            alloc.assignments().is_empty(),
            "a speaker who left the room is not still assigned a slot"
        );
        assert!(
            !alloc.remove_track(&speaker.track),
            "removing them twice is not a change"
        );
    }

    #[test]
    fn duplicate_mid_is_ignored() {
        let mut alloc = AudioAllocator::new(test_ctx());
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
