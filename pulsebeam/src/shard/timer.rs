use slotmap::SecondaryMap;
use std::{array, time::Duration};
use tokio::time::Instant;

use super::participants::ParticipantKey;

const SLOT_COUNT: usize = 256;
const OCCUPANCY_WORDS: usize = SLOT_COUNT / u64::BITS as usize;
#[allow(
    clippy::cast_possible_truncation,
    reason = "SLOT_COUNT is 256, asserted below"
)]
const DUE_LOCATION: u16 = SLOT_COUNT as u16;
const _: () = assert!(SLOT_COUNT == 256, "slot_of() relies on a 256-slot wheel");
const DISARMED_LOCATION: u16 = DUE_LOCATION + 1;
const MAX_DEADLINE_TICKS: u64 = 101;

/// The wheel slot a tick falls in.
///
/// The wheel has exactly `SLOT_COUNT` slots, so this is the tick modulo the
/// wheel size. The wrap is the addressing scheme, not a lost value.
fn slot_of(tick: u64) -> u16 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the 256-slot mask fits in u16"
    )]
    {
        (tick & 255) as u16
    }
}

fn duration_for_ticks(ticks: u64) -> Duration {
    let quantum_nanos =
        u64::try_from(pulsebeam_runtime::SHARD_TIMER_QUANTUM.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(ticks.saturating_mul(quantum_nanos))
}

#[derive(Clone, Copy)]
struct TimerNode {
    deadline_tick: u64,
    prev: Option<ParticipantKey>,
    next: Option<ParticipantKey>,
    location: u16,
}

impl TimerNode {
    fn new() -> Self {
        Self {
            deadline_tick: 0,
            prev: None,
            next: None,
            location: DISARMED_LOCATION,
        }
    }

    fn is_armed(&self) -> bool {
        self.location != DISARMED_LOCATION
    }
}

pub struct TimerWheel {
    epoch: Instant,
    last_now: Instant,
    current_tick: u64,
    heads: Box<[Option<ParticipantKey>; SLOT_COUNT]>,
    due_head: Option<ParticipantKey>,
    occupied: [u64; OCCUPANCY_WORDS],
    nodes: SecondaryMap<ParticipantKey, TimerNode>,
}

impl TimerWheel {
    pub fn new(capacity: usize) -> Self {
        let epoch = Instant::now();
        Self {
            epoch,
            last_now: epoch,
            current_tick: 0,
            heads: Box::new(array::from_fn(|_| None)),
            due_head: None,
            occupied: [0; OCCUPANCY_WORDS],
            nodes: SecondaryMap::with_capacity(capacity),
        }
    }

    pub fn schedule(&mut self, key: ParticipantKey, deadline: Instant) {
        let (location, deadline_tick) = if deadline <= self.last_now {
            (DUE_LOCATION, self.current_tick)
        } else {
            let requested_tick = self.deadline_tick(deadline);
            let ticks_until_deadline = requested_tick.saturating_sub(self.current_tick);
            debug_assert!(
                ticks_until_deadline <= MAX_DEADLINE_TICKS,
                "participant deadline exceeds the 100ms scheduling bound"
            );
            let deadline_tick = requested_tick.min(
                self.current_tick
                    .saturating_add((SLOT_COUNT as u64).saturating_sub(1)),
            );
            (slot_of(deadline_tick), deadline_tick)
        };

        if let Some(node) = self.nodes.get(key) {
            if node.location == location && node.deadline_tick == deadline_tick {
                return;
            }
            if node.is_armed() {
                self.unlink(key);
            }
        } else {
            let previous = self.nodes.insert(key, TimerNode::new());
            debug_assert!(previous.is_none());
        }

        self.link(key, location, deadline_tick);
    }

    pub fn cancel(&mut self, key: ParticipantKey) {
        let Some(node) = self.nodes.get(key) else {
            return;
        };
        if node.is_armed() {
            self.unlink(key);
        }
        let removed = self.nodes.remove(key);
        debug_assert!(removed.is_some());
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if self.due_head.is_some() {
            return Some(self.last_now);
        }

        let start = usize::from(slot_of(self.current_tick.saturating_add(1)));
        let offset = self.next_occupied_from(start)?;
        let tick = self.current_tick.saturating_add(1).saturating_add(offset);
        Some(
            self.epoch
                .checked_add(duration_for_ticks(tick))
                .unwrap_or(self.epoch),
        )
    }

    pub fn drain_expired(&mut self, now: Instant, mut f: impl FnMut(ParticipantKey)) {
        debug_assert!(now >= self.last_now, "timer clock moved backwards");
        self.drain_location(DUE_LOCATION, &mut f);

        let target_tick = self.elapsed_ticks(now);
        if target_tick.saturating_sub(self.current_tick) >= SLOT_COUNT as u64 {
            self.current_tick = target_tick;
            for slot in 0..SLOT_COUNT {
                self.drain_location(u16::try_from(slot).unwrap_or(DISARMED_LOCATION), &mut f);
            }
        } else {
            while self.current_tick < target_tick {
                self.current_tick = self.current_tick.saturating_add(1);
                self.drain_location(slot_of(self.current_tick), &mut f);
            }
        }
        debug_assert_eq!(self.current_tick, target_tick);
        self.last_now = now;
    }

    fn drain_location(&mut self, location: u16, f: &mut impl FnMut(ParticipantKey)) {
        loop {
            let id = if location == DUE_LOCATION {
                self.due_head
            } else {
                self.heads.get(usize::from(location)).copied().flatten()
            };
            let Some(id) = id else {
                break;
            };
            let Some(&node) = self.nodes.get(id) else {
                pulsebeam_runtime::fatal!("timer slot lists a node the wheel does not hold")
            };
            debug_assert_eq!(node.location, location);
            if location != DUE_LOCATION {
                debug_assert!(
                    node.deadline_tick <= self.current_tick
                        || self.current_tick.saturating_sub(node.deadline_tick)
                            >= SLOT_COUNT as u64
                );
            }
            self.unlink(id);
            f(id);
        }
    }

    fn link(&mut self, id: ParticipantKey, location: u16, deadline_tick: u64) {
        debug_assert!(location <= DUE_LOCATION);
        let old_head = if location == DUE_LOCATION {
            self.due_head
        } else {
            self.heads.get(usize::from(location)).copied().flatten()
        };

        {
            let Some(node) = self.nodes.get_mut(id) else {
                pulsebeam_runtime::fatal!(
                    "arming a timer for a participant the wheel does not hold"
                )
            };
            debug_assert!(!node.is_armed());
            node.deadline_tick = deadline_tick;
            node.prev = None;
            node.next = old_head;
            node.location = location;
        }
        if let Some(old_head) = old_head {
            let Some(head) = self.nodes.get_mut(old_head) else {
                pulsebeam_runtime::fatal!(
                    "timer slot head points at a node the wheel does not hold"
                )
            };
            debug_assert_eq!(head.location, location);
            debug_assert!(head.prev.is_none());
            head.prev = Some(id);
        }

        if location == DUE_LOCATION {
            self.due_head = Some(id);
        } else {
            let slot = usize::from(location);
            if let Some(head) = self.heads.get_mut(slot) {
                *head = Some(id);
                self.set_occupied(slot);
            } else {
                debug_assert!(false, "link target {location} is not a wheel slot");
            }
        }
    }

    fn unlink(&mut self, id: ParticipantKey) {
        let Some(&node) = self.nodes.get(id) else {
            pulsebeam_runtime::fatal!("unlinking a timer the wheel does not hold")
        };
        debug_assert!(node.is_armed());

        if let Some(prev) = node.prev {
            let Some(prev_node) = self.nodes.get_mut(prev) else {
                pulsebeam_runtime::fatal!(
                    "timer list back-link points at a node the wheel does not hold"
                )
            };
            debug_assert_eq!(prev_node.location, node.location);
            debug_assert_eq!(prev_node.next.map(|v| v == id), Some(true));
            prev_node.next = node.next;
        } else if node.location == DUE_LOCATION {
            debug_assert_eq!(self.due_head.map(|v| v == id), Some(true));
            self.due_head = node.next;
        } else {
            let slot = usize::from(node.location);
            if let Some(head) = self.heads.get_mut(slot) {
                debug_assert_eq!(head.map(|v| v == id), Some(true));
                *head = node.next;
            } else {
                debug_assert!(false, "unlink target {slot} is not a wheel slot");
            }
        }

        if let Some(next) = node.next {
            let Some(next_node) = self.nodes.get_mut(next) else {
                pulsebeam_runtime::fatal!(
                    "timer list forward-link points at a node the wheel does not hold"
                )
            };
            debug_assert_eq!(next_node.location, node.location);
            debug_assert_eq!(next_node.prev.map(|v| v == id), Some(true));
            next_node.prev = node.prev;
        }

        if node.location != DUE_LOCATION {
            let slot = usize::from(node.location);
            if self.heads.get(slot).copied().flatten().is_none() {
                self.clear_occupied(slot);
            }
        }

        let Some(node) = self.nodes.get_mut(id) else {
            pulsebeam_runtime::fatal!("clearing a timer the wheel does not hold")
        };
        node.prev = None;
        node.next = None;
        node.location = DISARMED_LOCATION;
    }

    fn deadline_tick(&self, deadline: Instant) -> u64 {
        let elapsed = deadline.saturating_duration_since(self.epoch);
        let quantum = pulsebeam_runtime::SHARD_TIMER_QUANTUM.as_nanos();
        debug_assert_ne!(quantum, 0);
        let rounded = elapsed.as_nanos().div_ceil(quantum);
        u64::try_from(rounded).unwrap_or(u64::MAX)
    }

    fn elapsed_ticks(&self, now: Instant) -> u64 {
        let quantum = pulsebeam_runtime::SHARD_TIMER_QUANTUM.as_nanos();
        debug_assert_ne!(quantum, 0);
        let ticks = now
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .checked_div(quantum)
            .unwrap_or(u128::MAX);
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }

    #[cfg(test)]
    fn slot_occupied(&self, slot: usize) -> bool {
        self.occupied
            .get(slot / 64)
            .is_some_and(|word| word & (1 << (slot % 64)) != 0)
    }

    fn next_occupied_from(&self, start: usize) -> Option<u64> {
        debug_assert!(start < SLOT_COUNT);
        for step in 0..OCCUPANCY_WORDS.saturating_add(1) {
            let base = start.saturating_add(step.saturating_mul(64));
            let word = self.rotated_word(base);
            if word != 0 {
                let distance = step
                    .saturating_mul(64)
                    .saturating_add(word.trailing_zeros() as usize);
                if distance < SLOT_COUNT {
                    return Some(distance as u64);
                }
            }
        }
        None
    }

    /// The 64 slots beginning at `base` (wrapping), packed into a word with
    /// `base` at bit 0.
    fn rotated_word(&self, base: usize) -> u64 {
        let base = base % SLOT_COUNT;
        let word_idx = base / 64;
        let bit = base % 64;
        let lo = self.occupied.get(word_idx).copied().unwrap_or(0);
        let hi = self
            .occupied
            .get(word_idx.saturating_add(1) % OCCUPANCY_WORDS)
            .copied()
            .unwrap_or(0);
        if bit == 0 {
            lo
        } else {
            // `bit` is `base % 64` and non-zero here, so the complement is in
            // 1..=63 and neither shift can reach the width. Spelled with
            // `saturating_sub` because this module denies bare arithmetic: a
            // wrapped shift width here would silently mis-read the wheel
            // rather than fail.
            debug_assert!((1..64).contains(&bit));
            let complement = 64usize.saturating_sub(bit);
            (lo >> bit) | (hi << complement)
        }
    }

    fn set_occupied(&mut self, slot: usize) {
        if let Some(word) = self.occupied.get_mut(slot / 64) {
            *word |= 1 << (slot % 64);
        }
    }

    fn clear_occupied(&mut self, slot: usize) {
        if let Some(word) = self.occupied.get_mut(slot / 64) {
            *word &= !(1 << (slot % 64));
        }
    }
}

#[cfg(test)]
mod tests {

    /// Word boundaries and their neighbours, plus the wrap point: the slots
    /// where scanning a word at a time can disagree with scanning a slot at a
    /// time. Derived from `SLOT_COUNT` rather than written out, so resizing the
    /// wheel cannot leave this pointing at slots that no longer exist.
    fn boundary_slots() -> Vec<usize> {
        let mut slots = vec![0, 1];
        for word_start in (64..SLOT_COUNT).step_by(64) {
            slots.extend([word_start - 1, word_start, word_start + 1]);
        }
        slots.push(SLOT_COUNT - 1);
        slots.retain(|slot| *slot < SLOT_COUNT);
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    #[test]
    fn word_scanning_agrees_with_a_naive_slot_scan() {
        fn naive(wheel: &TimerWheel, start: usize) -> Option<u64> {
            (0..SLOT_COUNT).find_map(|d| {
                wheel
                    .slot_occupied((start + d) % SLOT_COUNT)
                    .then_some(d as u64)
            })
        }

        let mut wheel = TimerWheel::new(8);
        for start in 0..SLOT_COUNT {
            assert_eq!(
                wheel.next_occupied_from(start),
                naive(&wheel, start),
                "empty wheel, start {start}"
            );
        }

        for armed in 0..SLOT_COUNT {
            wheel.set_occupied(armed);
            for start in boundary_slots() {
                assert_eq!(
                    wheel.next_occupied_from(start),
                    naive(&wheel, start),
                    "slot {armed} armed, start {start}"
                );
            }
            wheel.clear_occupied(armed);
        }

        // A scattered pattern that straddles every word boundary.
        for armed in boundary_slots() {
            wheel.set_occupied(armed);
        }
        for start in 0..SLOT_COUNT {
            assert_eq!(
                wheel.next_occupied_from(start),
                naive(&wheel, start),
                "scattered pattern, start {start}"
            );
        }
    }
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    fn keys(count: u8) -> Vec<ParticipantKey> {
        (0..count)
            .map(|index| {
                ParticipantKey::from(slotmap::KeyData::from_ffi((1_u64 << 32) | u64::from(index)))
            })
            .collect()
    }

    #[test]
    fn deadlines_fire_once_after_their_ceiling_tick() {
        let mut wheel = TimerWheel::new(16);
        let key = keys(1)[0];
        let start = wheel.epoch;
        wheel.schedule(key, start + Duration::from_micros(1_001));

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_micros(1_999), |entry| {
            expired.push(entry);
        });
        assert!(expired.is_empty());
        wheel.drain_expired(start + Duration::from_millis(2), |entry| {
            expired.push(entry);
        });
        assert_eq!(expired, vec![key]);
        assert_eq!(wheel.next_deadline(), None);
    }

    #[test]
    fn rescheduling_replaces_the_only_entry() {
        let mut wheel = TimerWheel::new(16);
        let key = keys(1)[0];
        let start = wheel.epoch;
        wheel.schedule(key, start + Duration::from_millis(20));
        wheel.schedule(key, start + Duration::from_millis(40));
        wheel.schedule(key, start + Duration::from_millis(10));

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |entry| {
            expired.push(entry);
        });
        assert_eq!(expired, vec![key]);
        wheel.drain_expired(start + Duration::from_millis(50), |entry| {
            expired.push(entry);
        });
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn due_deadlines_do_not_wrap() {
        let mut wheel = TimerWheel::new(16);
        let key = keys(1)[0];
        let start = wheel.epoch;
        wheel.drain_expired(start + Duration::from_millis(10), |_| {});
        wheel.schedule(key, start + Duration::from_millis(9));

        assert_eq!(
            wheel.next_deadline(),
            Some(start + Duration::from_millis(10))
        );
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |entry| {
            expired.push(entry);
        });
        assert_eq!(expired, vec![key]);
    }

    #[test]
    fn cancellation_removes_only_the_target() {
        let mut wheel = TimerWheel::new(16);
        let keys = keys(2);
        let start = wheel.epoch;
        wheel.schedule(keys[0], start + Duration::from_millis(10));
        wheel.schedule(keys[1], start + Duration::from_millis(10));
        wheel.cancel(keys[0]);

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |entry| {
            expired.push(entry);
        });
        assert_eq!(expired, vec![keys[1]]);
    }

    #[test]
    fn late_advance_expires_every_armed_participant_once() {
        let mut wheel = TimerWheel::new(16);
        let keys = keys(16);
        let start = wheel.epoch;
        for (offset, key) in keys.iter().copied().enumerate() {
            wheel.schedule(key, start + Duration::from_millis(offset as u64));
        }

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_secs(1), |entry| {
            expired.push(entry);
        });
        expired.sort_unstable();
        let mut expected = keys;
        expected.sort_unstable();
        assert_eq!(expired, expected);
    }
}
