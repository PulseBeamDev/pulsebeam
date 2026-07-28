use ahash::{HashMap, HashMapExt};
use std::{array, hash::Hash, time::Duration};
use tokio::time::Instant;

const SLOT_COUNT: usize = 256;
const OCCUPANCY_WORDS: usize = SLOT_COUNT / u64::BITS as usize;
const DUE_LOCATION: u16 = SLOT_COUNT as u16;
const DISARMED_LOCATION: u16 = DUE_LOCATION + 1;
const MAX_DEADLINE_TICKS: u64 = 101;

#[derive(Clone, Copy)]
struct TimerNode<T> {
    generation: u64,
    deadline_tick: u64,
    prev: Option<T>,
    next: Option<T>,
    location: u16,
}

impl<T> TimerNode<T> {
    fn new(generation: u64) -> Self {
        Self {
            generation,
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

pub struct TimerWheel<T: Eq + Hash + Copy> {
    epoch: Instant,
    last_now: Instant,
    current_tick: u64,
    heads: [Option<T>; SLOT_COUNT],
    due_head: Option<T>,
    occupied: [u64; OCCUPANCY_WORDS],
    nodes: HashMap<T, TimerNode<T>>,
}

impl<T: Eq + Hash + Copy> TimerWheel<T> {
    pub fn new(capacity: usize) -> Self {
        let epoch = Instant::now();
        Self {
            epoch,
            last_now: epoch,
            current_tick: 0,
            heads: array::from_fn(|_| None),
            due_head: None,
            occupied: [0; OCCUPANCY_WORDS],
            nodes: HashMap::with_capacity(capacity),
        }
    }

    pub fn schedule(&mut self, id: T, generation: u64, deadline: Instant) {
        debug_assert_ne!(generation, 0);
        let (location, deadline_tick) = if deadline <= self.last_now {
            (DUE_LOCATION, self.current_tick)
        } else {
            let requested_tick = self.deadline_tick(deadline);
            let ticks_until_deadline = requested_tick.saturating_sub(self.current_tick);
            debug_assert!(
                ticks_until_deadline <= MAX_DEADLINE_TICKS,
                "participant deadline exceeds the 100ms scheduling bound"
            );
            let deadline_tick = requested_tick.min(self.current_tick + (SLOT_COUNT as u64 - 1));
            (u16::from(deadline_tick as u8), deadline_tick)
        };

        if let Some(node) = self.nodes.get(&id) {
            debug_assert_eq!(
                node.generation, generation,
                "timer generation changed without cancellation"
            );
            if node.location == location && node.deadline_tick == deadline_tick {
                return;
            }
            if node.is_armed() {
                self.unlink(id);
            }
        } else {
            self.nodes.insert(id, TimerNode::new(generation));
        }

        self.link(id, location, deadline_tick);
    }

    pub fn cancel(&mut self, id: &T) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        if node.is_armed() {
            self.unlink(*id);
        }
        let removed = self.nodes.remove(id);
        debug_assert!(removed.is_some());
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if self.due_head.is_some() {
            return Some(self.last_now);
        }

        let mut offset = 1;
        while offset < SLOT_COUNT {
            let tick = self.current_tick + offset as u64;
            if self.slot_occupied(tick as u8) {
                return Some(self.epoch + Duration::from_millis(tick));
            }
            offset += 1;
        }
        None
    }

    pub fn drain_expired(&mut self, now: Instant, mut f: impl FnMut(T, u64)) {
        debug_assert!(now >= self.last_now, "timer clock moved backwards");
        self.drain_location(DUE_LOCATION, &mut f);

        let target_tick = self.elapsed_ticks(now);
        if target_tick.saturating_sub(self.current_tick) >= SLOT_COUNT as u64 {
            self.current_tick = target_tick;
            for slot in 0..SLOT_COUNT {
                self.drain_location(slot as u16, &mut f);
            }
        } else {
            while self.current_tick < target_tick {
                self.current_tick += 1;
                self.drain_location(u16::from(self.current_tick as u8), &mut f);
            }
        }
        debug_assert_eq!(self.current_tick, target_tick);
        self.last_now = now;
    }

    fn drain_location(&mut self, location: u16, f: &mut impl FnMut(T, u64)) {
        loop {
            let id = if location == DUE_LOCATION {
                self.due_head
            } else {
                self.heads[location as usize]
            };
            let Some(id) = id else {
                break;
            };
            let node = *self.nodes.get(&id).expect("armed timer node missing");
            debug_assert_eq!(node.location, location);
            if location != DUE_LOCATION {
                debug_assert!(
                    node.deadline_tick <= self.current_tick
                        || self.current_tick.saturating_sub(node.deadline_tick)
                            >= SLOT_COUNT as u64
                );
            }
            self.unlink(id);
            f(id, node.generation);
        }
    }

    fn link(&mut self, id: T, location: u16, deadline_tick: u64) {
        debug_assert!(location <= DUE_LOCATION);
        let old_head = if location == DUE_LOCATION {
            self.due_head
        } else {
            self.heads[location as usize]
        };

        {
            let node = self.nodes.get_mut(&id).expect("timer node missing");
            debug_assert!(!node.is_armed());
            node.deadline_tick = deadline_tick;
            node.prev = None;
            node.next = old_head;
            node.location = location;
        }
        if let Some(old_head) = old_head {
            let head = self
                .nodes
                .get_mut(&old_head)
                .expect("timer slot head missing");
            debug_assert_eq!(head.location, location);
            debug_assert!(head.prev.is_none());
            head.prev = Some(id);
        }

        if location == DUE_LOCATION {
            self.due_head = Some(id);
        } else {
            let slot = location as usize;
            self.heads[slot] = Some(id);
            self.set_occupied(slot as u8);
        }
    }

    fn unlink(&mut self, id: T) {
        let node = *self.nodes.get(&id).expect("timer node missing");
        debug_assert!(node.is_armed());

        if let Some(prev) = node.prev {
            let prev_node = self.nodes.get_mut(&prev).expect("previous timer missing");
            debug_assert_eq!(prev_node.location, node.location);
            debug_assert_eq!(prev_node.next.map(|v| v == id), Some(true));
            prev_node.next = node.next;
        } else if node.location == DUE_LOCATION {
            debug_assert_eq!(self.due_head.map(|v| v == id), Some(true));
            self.due_head = node.next;
        } else {
            let slot = node.location as usize;
            debug_assert_eq!(self.heads[slot].map(|v| v == id), Some(true));
            self.heads[slot] = node.next;
        }

        if let Some(next) = node.next {
            let next_node = self.nodes.get_mut(&next).expect("next timer missing");
            debug_assert_eq!(next_node.location, node.location);
            debug_assert_eq!(next_node.prev.map(|v| v == id), Some(true));
            next_node.prev = node.prev;
        }

        if node.location != DUE_LOCATION {
            let slot = node.location as usize;
            if self.heads[slot].is_none() {
                self.clear_occupied(slot as u8);
            }
        }

        let node = self.nodes.get_mut(&id).expect("timer node missing");
        node.prev = None;
        node.next = None;
        node.location = DISARMED_LOCATION;
    }

    fn deadline_tick(&self, deadline: Instant) -> u64 {
        let elapsed = deadline.saturating_duration_since(self.epoch);
        let millis = elapsed.as_millis();
        let rounded = millis + u128::from(!elapsed.subsec_nanos().is_multiple_of(1_000_000));
        u64::try_from(rounded).unwrap_or(u64::MAX)
    }

    fn elapsed_ticks(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.epoch).as_millis()).unwrap_or(u64::MAX)
    }

    fn slot_occupied(&self, slot: u8) -> bool {
        let slot = slot as usize;
        self.occupied[slot / 64] & (1 << (slot % 64)) != 0
    }

    fn set_occupied(&mut self, slot: u8) {
        let slot = slot as usize;
        self.occupied[slot / 64] |= 1 << (slot % 64);
    }

    fn clear_occupied(&mut self, slot: u8) {
        let slot = slot as usize;
        self.occupied[slot / 64] &= !(1 << (slot % 64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel() -> TimerWheel<u32> {
        TimerWheel::new(16)
    }

    #[test]
    fn deadlines_fire_once_after_their_ceiling_tick() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        wheel.schedule(1, 1, start + Duration::from_micros(1_001));

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_micros(1_999), |id, _| {
            expired.push(id)
        });
        assert!(expired.is_empty());
        wheel.drain_expired(start + Duration::from_millis(2), |id, _| expired.push(id));
        assert_eq!(expired, vec![1]);
        assert_eq!(wheel.next_deadline(), None);
    }

    #[test]
    fn rescheduling_replaces_the_only_entry() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        wheel.schedule(1, 7, start + Duration::from_millis(20));
        wheel.schedule(1, 7, start + Duration::from_millis(40));
        wheel.schedule(1, 7, start + Duration::from_millis(10));

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |id, generation| {
            expired.push((id, generation))
        });
        assert_eq!(expired, vec![(1, 7)]);
        wheel.drain_expired(start + Duration::from_millis(50), |id, generation| {
            expired.push((id, generation))
        });
        assert_eq!(expired, vec![(1, 7)]);
    }

    #[test]
    fn due_deadlines_do_not_wrap() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        wheel.drain_expired(start + Duration::from_millis(10), |_, _| {});
        wheel.schedule(1, 1, start + Duration::from_millis(9));

        assert_eq!(
            wheel.next_deadline(),
            Some(start + Duration::from_millis(10))
        );
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |id, _| expired.push(id));
        assert_eq!(expired, vec![1]);
    }

    #[test]
    fn cancellation_removes_only_the_target() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        wheel.schedule(1, 1, start + Duration::from_millis(10));
        wheel.schedule(2, 2, start + Duration::from_millis(10));
        wheel.cancel(&1);

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(10), |id, generation| {
            expired.push((id, generation))
        });
        assert_eq!(expired, vec![(2, 2)]);
    }

    #[test]
    fn cancelled_id_can_be_reused_with_a_new_generation() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        wheel.schedule(1, 1, start + Duration::from_millis(10));
        wheel.cancel(&1);
        wheel.schedule(1, 2, start + Duration::from_millis(20));

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(20), |id, generation| {
            expired.push((id, generation))
        });
        assert_eq!(expired, vec![(1, 2)]);
    }

    #[test]
    fn late_advance_expires_every_armed_participant_once() {
        let mut wheel = wheel();
        let start = wheel.epoch;
        for id in 0..16 {
            wheel.schedule(
                id,
                u64::from(id) + 1,
                start + Duration::from_millis(id.into()),
            );
        }

        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_secs(1), |id, generation| {
            expired.push((id, generation))
        });
        expired.sort_unstable();
        assert_eq!(
            expired,
            (0..16)
                .map(|id| (id, u64::from(id) + 1))
                .collect::<Vec<_>>()
        );
    }
}
