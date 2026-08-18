use std::array;
use std::time::Duration;

use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::control::MAX_SEND_AUDIO_SLOTS;
use crate::entity::{AudioOrigin, TrackId};
use crate::log::{LogCtx, plog_debug, plog_warn};
use crate::participant::downstream::SlotConfig;
use crate::rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline};
use crate::track::StreamWriter;

/// One subscriber's audio slots, and the contest for them.
///
/// The number of slots is the audio mid count negotiated for this client, so both the capacity
/// and the ranking that fills it belong to the listener. They used to be split: a shard-wide
/// `TopNAudioSelector` chose a slot index and this type mapped it onto a mid. That gave one
/// three-slot contest to every room on the shard at once, so a loud room silenced the quiet ones -
/// see `tests::room_isolation::audio_selection_does_not_cross_rooms_test`. A listener only ever
/// ranks the streams forwarded to it, which are its own room's, so scoping is a property here
/// rather than a check.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// A slot is dead if no packet has arrived within this window.
const DEAD_TIMEOUT: Duration = Duration::from_millis(2000);
/// After a slot is stolen, this protects the new owner from delayed packets from the previous one.
const NEWBORN_IMMUNITY: Duration = Duration::from_millis(300);
/// Half-life for contention power decay: keeps ranking relative without letting one quiet packet
/// (a transient DTX/silence frame) thrash a slot.
const POWER_HALF_LIFE: Duration = Duration::from_millis(300);

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
    /// Slots are stolen as people start and stop talking, so a subscriber cannot work out who it
    /// is hearing from the media: every slot is one continuous stream on one mid whoever is
    /// speaking. Recording it here is what lets the assignment be signalled.
    occupant: Option<Occupant>,
    /// Wall-clock time of the most recent packet. `None` means never used.
    last_arrival_ts: Option<Instant>,
    /// Wall-clock time until which this slot cannot be stolen.
    immunity_expiry: Instant,
    /// Decayed power of the most recent packet, for relative ranking.
    last_power: f32,
    /// Every occupant is rewritten onto this, so a steal does not tear the stream.
    timeline: Timeline,
}

impl Slot {
    /// No owner, or an owner silent for longer than [`DEAD_TIMEOUT`].
    fn is_dead(&self, now: Instant) -> bool {
        match (self.occupant, self.last_arrival_ts) {
            (None, _) | (Some(_), None) => true,
            (Some(_), Some(ts)) => now.duration_since(ts) > DEAD_TIMEOUT,
        }
    }

    fn is_immune(&self, now: Instant) -> bool {
        now < self.immunity_expiry
    }
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
                    last_arrival_ts: None,
                    immunity_expiry: Instant::now(),
                    last_power: 0.0,
                    timeline: Timeline::new(AUDIO_FREQUENCY),
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

    /// Place a packet in this subscriber's slots, or drop it for losing the contest.
    ///
    /// The owner of a slot is never made to re-contend for it, so a speaker holds their slot while
    /// they keep talking. A newcomer takes a dead slot if there is one, and otherwise the quietest
    /// slot not under newborn immunity, and only if it is genuinely louder than what is there.
    pub fn on_rtp(
        &mut self,
        origin: AudioOrigin,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> Option<()> {
        let Some(level_dbov) = pkt.ext_vals.audio_level else {
            plog_warn!(
                self.ctx,
                target: crate::log::TARGET_AUDIO,
                track = ?origin.track,
                "audio packet dropped for want of an RFC 6464 level"
            );
            return None;
        };
        let power = rfc6464_to_power(level_dbov);
        let now = pkt.arrival_ts;

        for slot in self.slots.iter_mut().flatten() {
            slot.last_power = decayed_power(slot.last_power, slot.last_arrival_ts, now);
        }

        let idx = self.slot_for(origin, power, now)?;
        let slot = self.slots.get_mut(idx).and_then(Option::as_mut)?;

        let previous = slot.occupant;
        let switched = previous.map(|o| o.origin) != Some(origin);
        self.speakers_changed |= switched;
        slot.occupant = Some(Occupant { origin, level_dbov });
        slot.last_arrival_ts = Some(now);
        // Peak-hold with decay: one quiet packet must not instantly demote a talker's rank.
        slot.last_power = if switched {
            power
        } else {
            slot.last_power.max(power)
        };

        let mut pkt = pkt.clone();
        if switched {
            // Immunity fends off packets still in flight from whoever was displaced. A slot that
            // never had an occupant has nobody in flight, and holding one there only delays the
            // ranking from settling while everyone is still arriving.
            if previous.is_some() {
                slot.immunity_expiry = now.checked_add(NEWBORN_IMMUNITY).unwrap_or(now);
            }
            slot.timeline.rebase_audio(&pkt);
            slot.pending_marker = true;
        }
        slot.timeline.rewrite(&mut pkt);
        if slot.pending_marker {
            pkt.marker = true;
            slot.pending_marker = false;
        }
        writer.write_audio_owned(pkt, slot.mid, slot.ssrc, slot.pt);
        Some(())
    }

    /// Which slot this speaker gets, if any.
    fn slot_for(&self, origin: AudioOrigin, power: f32, now: Instant) -> Option<usize> {
        if let Some((idx, _)) = self
            .provisioned()
            .find(|(_, slot)| slot.occupant.map(|o| o.origin) == Some(origin))
        {
            return Some(idx);
        }
        if let Some((idx, _)) = self
            .provisioned()
            .find(|(_, slot)| slot.is_dead(now) && !slot.is_immune(now))
        {
            return Some(idx);
        }
        let (idx, quietest) = self
            .provisioned()
            .filter(|(_, slot)| !slot.is_immune(now))
            .min_by(|(_, a), (_, b)| {
                a.last_power
                    .partial_cmp(&b.last_power)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
        if power > quietest.last_power {
            return Some(idx);
        }
        plog_debug!(
            self.ctx,
            target: crate::log::TARGET_AUDIO,
            track = ?origin.track,
            incoming_power = power,
            quietest_power = quietest.last_power,
            "audio packet dropped in contention"
        );
        None
    }

    fn provisioned(&self) -> impl Iterator<Item = (usize, &Slot)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| slot.as_ref().map(|slot| (idx, slot)))
    }
}

#[inline(always)]
fn rfc6464_to_power(level: i8) -> f32 {
    let clamped = level.clamp(-127, 0) as f32;
    10.0_f32.powf(clamped / 10.0)
}

#[inline(always)]
fn decayed_power(power: f32, last_arrival_ts: Option<Instant>, now: Instant) -> f32 {
    let Some(last) = last_arrival_ts else {
        return 0.0;
    };
    let dt = now
        .checked_duration_since(last)
        .unwrap_or(Duration::from_millis(0));
    if dt.is_zero() {
        return power;
    }
    let half_life = POWER_HALF_LIFE.as_secs_f32();
    power * 0.5_f32.powf(dt.as_secs_f32() / half_life)
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::participant::downstream::SlotConfig;
    use str0m::media::{MediaKind, Mid, Pt};
    use str0m::rtp::Ssrc;

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, ParticipantId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(),
        }
    }

    fn origin(seed: u64) -> AudioOrigin {
        use crate::entity::{ParticipantId, TrackKind};
        let _rng = pulsebeam_runtime::rand::seeded_rng(seed);
        let participant = ParticipantId::new();
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

    fn speaking_at_time(level_dbov: i8, at: Instant) -> RtpPacket {
        let mut pkt = speaking(level_dbov);
        pkt.arrival_ts = at;
        pkt
    }

    fn slot_config(mid: &'static str, ssrc: u32) -> SlotConfig {
        SlotConfig {
            mid: Mid::from(mid),
            rid: None,
            ssrc: Ssrc::from(ssrc),
            pt: Pt::from(111_u8),
            kind: MediaKind::Audio,
        }
    }

    fn allocator_with(slots: usize) -> AudioAllocator {
        let mut alloc = AudioAllocator::new(test_ctx());
        for (mid, ssrc) in [("a0", 1000), ("a1", 1001), ("a2", 1002), ("a3", 1003)]
            .into_iter()
            .take(slots)
        {
            alloc.add_slot(slot_config(mid, ssrc));
        }
        alloc
    }

    fn heard_origins(alloc: &AudioAllocator) -> Vec<AudioOrigin> {
        alloc.assignments().into_iter().map(|h| h.origin).collect()
    }

    #[test]
    fn first_forwarded_packet_carries_the_talk_spurt_marker() {
        let mut alloc = allocator_with(1);
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "a new audio slot must start with a pending marker"
        );

        alloc.on_rtp(origin(1), &speaking(-30), &mut StreamWriter::new());
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "the first forwarded packet consumes the marker"
        );

        alloc.on_rtp(origin(1), &speaking(-30), &mut StreamWriter::new());
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| !s.pending_marker),
            "and it stays consumed for the rest of the spurt"
        );
    }

    /// A packet with no RFC 6464 level cannot be ranked, so it cannot be placed.
    ///
    /// Admitting it would mean guessing a loudness, and the guess decides who gets heard.
    #[test]
    fn a_packet_without_a_level_is_not_placed() {
        let mut alloc = allocator_with(1);
        assert!(
            alloc
                .on_rtp(origin(1), &RtpPacket::default(), &mut StreamWriter::new())
                .is_none()
        );
        assert!(heard_origins(&alloc).is_empty());
    }

    #[test]
    fn nothing_is_placed_when_no_slot_was_negotiated() {
        let mut alloc = allocator_with(0);
        assert!(
            alloc
                .on_rtp(origin(1), &speaking(-20), &mut StreamWriter::new())
                .is_none(),
            "a subscriber that negotiated no audio mid has nowhere to put a voice"
        );
    }

    /// The loudest speakers get the slots, and the quiet one is left out.
    ///
    /// The claim the whole type exists for: with more voices than slots, who is heard is decided
    /// by loudness rather than by who arrived first.
    #[test]
    fn the_loudest_speakers_take_the_slots() {
        let mut alloc = allocator_with(2);
        let (loud, louder, faint) = (origin(1), origin(2), origin(3));

        alloc.on_rtp(faint, &speaking(-75), &mut StreamWriter::new());
        alloc.on_rtp(loud, &speaking(-25), &mut StreamWriter::new());
        alloc.on_rtp(louder, &speaking(-20), &mut StreamWriter::new());

        let heard = heard_origins(&alloc);
        assert!(heard.contains(&loud), "the loud speaker is heard");
        assert!(heard.contains(&louder), "the louder speaker is heard");
        assert!(
            !heard.contains(&faint),
            "arriving first does not keep a faint speaker in a contested slot"
        );
    }

    /// A speaker who holds a slot is never made to re-contend for it.
    ///
    /// Without this a talker whose level dips below a rival's for one packet loses their slot,
    /// and the stream tears every time two people are at similar volume.
    #[test]
    fn a_slot_owner_keeps_it_while_they_keep_talking() {
        let mut alloc = allocator_with(1);
        let (holder, rival) = (origin(1), origin(2));
        let start = Instant::now();

        alloc.on_rtp(
            holder,
            &speaking_at_time(-30, start),
            &mut StreamWriter::new(),
        );
        // Past newborn immunity, so only the owner rule can be keeping the slot.
        let later = start + NEWBORN_IMMUNITY + Duration::from_millis(10);
        alloc.on_rtp(
            holder,
            &speaking_at_time(-30, later),
            &mut StreamWriter::new(),
        );

        assert_eq!(heard_origins(&alloc), vec![holder]);
        assert!(
            alloc
                .on_rtp(
                    rival,
                    &speaking_at_time(-31, later),
                    &mut StreamWriter::new()
                )
                .is_none(),
            "a quieter rival does not take an occupied slot"
        );
        assert_eq!(heard_origins(&alloc), vec![holder]);
    }

    /// A newly stolen slot is briefly protected from the speaker it was taken from.
    ///
    /// Packets already in flight from the previous owner arrive after the steal, and without
    /// immunity they take it straight back - the two swap the slot at network jitter frequency.
    #[test]
    fn a_freshly_taken_slot_is_immune_to_the_speaker_it_was_taken_from() {
        let mut alloc = allocator_with(1);
        let (first, second) = (origin(1), origin(2));
        let start = Instant::now();

        alloc.on_rtp(
            first,
            &speaking_at_time(-40, start),
            &mut StreamWriter::new(),
        );
        let steal_at = start + NEWBORN_IMMUNITY + Duration::from_millis(10);
        alloc.on_rtp(
            second,
            &speaking_at_time(-20, steal_at),
            &mut StreamWriter::new(),
        );
        assert_eq!(
            heard_origins(&alloc),
            vec![second],
            "the louder voice took it"
        );

        let delayed = steal_at + Duration::from_millis(10);
        alloc.on_rtp(
            first,
            &speaking_at_time(-10, delayed),
            &mut StreamWriter::new(),
        );
        assert_eq!(
            heard_origins(&alloc),
            vec![second],
            "a delayed packet cannot take the slot back inside the immunity window"
        );
    }

    /// A slot whose owner has gone silent is reclaimed rather than held forever.
    #[test]
    fn a_silent_slot_is_reclaimed_by_a_newcomer() {
        let mut alloc = allocator_with(1);
        let (departed, newcomer) = (origin(1), origin(2));
        let start = Instant::now();

        alloc.on_rtp(
            departed,
            &speaking_at_time(-20, start),
            &mut StreamWriter::new(),
        );
        // Quieter than the previous owner, so only the slot being dead can admit it.
        let after_silence = start + DEAD_TIMEOUT + Duration::from_millis(10);
        alloc.on_rtp(
            newcomer,
            &speaking_at_time(-60, after_silence),
            &mut StreamWriter::new(),
        );

        assert_eq!(heard_origins(&alloc), vec![newcomer]);
    }

    #[test]
    fn who_is_heard_is_ranked_by_loudness() {
        let mut alloc = allocator_with(2);
        let (quiet, loud) = (origin(1), origin(2));

        alloc.on_rtp(quiet, &speaking(-45), &mut StreamWriter::new());
        alloc.on_rtp(loud, &speaking(-20), &mut StreamWriter::new());

        assert_eq!(
            heard_origins(&alloc),
            vec![loud, quiet],
            "rank 0 is the loudest, which is what a UI highlights"
        );
    }

    /// Only a change of speaker is a change worth signalling.
    ///
    /// Loudness moves with every packet, so treating it as a trigger would emit a signalling
    /// message per packet in any room where two people talk at similar volume.
    #[test]
    fn only_a_new_occupant_counts_as_a_change() {
        let mut alloc = allocator_with(1);
        let speaker = origin(1);

        alloc.on_rtp(speaker, &speaking(-30), &mut StreamWriter::new());
        assert!(
            alloc.take_speakers_changed(),
            "the first occupant is a change"
        );

        alloc.on_rtp(speaker, &speaking(-20), &mut StreamWriter::new());
        assert!(
            !alloc.take_speakers_changed(),
            "the same speaker getting louder is not"
        );
    }

    /// A speaker who has left stops being announced.
    ///
    /// A slot holds its last occupant while they are merely quiet, which is right. Holding one
    /// who has gone leaves the room showing a tile for somebody who is not in it.
    #[test]
    fn a_departed_speaker_stops_occupying_their_slot() {
        let mut alloc = allocator_with(1);
        let speaker = origin(1);

        alloc.on_rtp(speaker, &speaking(-30), &mut StreamWriter::new());
        assert_eq!(heard_origins(&alloc), vec![speaker]);

        assert!(alloc.remove_track(&speaker.track));
        assert!(heard_origins(&alloc).is_empty());
        assert!(
            alloc.slots[0].as_ref().is_some_and(|s| s.pending_marker),
            "whoever takes the slot next starts a talk spurt"
        );
    }

    #[test]
    fn duplicate_mid_is_ignored() {
        let mut alloc = allocator_with(0);
        alloc.add_slot(slot_config("a0", 1000));
        alloc.add_slot(slot_config("a0", 2000));
        assert_eq!(
            alloc.slots.iter().flatten().count(),
            1,
            "one mid is one slot, however many times it is offered"
        );
    }

    /// Contention is scoped to what this listener was given, and nothing else.
    ///
    /// The regression guard for a shard-wide selector: three loud voices saturating one
    /// subscriber's slots are not even visible to another subscriber, so they cannot displace
    /// anybody there. Two allocators is the unit-level shape of two rooms on one shard.
    #[test]
    fn one_listeners_contention_does_not_reach_another() {
        let mut busy = allocator_with(1);
        let mut calm = allocator_with(1);
        let (shouter, quiet_speaker) = (origin(1), origin(2));

        for _ in 0..8 {
            busy.on_rtp(shouter, &speaking(-10), &mut StreamWriter::new());
        }
        calm.on_rtp(quiet_speaker, &speaking(-70), &mut StreamWriter::new());

        assert_eq!(heard_origins(&busy), vec![shouter]);
        assert_eq!(
            heard_origins(&calm),
            vec![quiet_speaker],
            "a very quiet voice still gets a slot where nothing competes for one"
        );
    }
}
