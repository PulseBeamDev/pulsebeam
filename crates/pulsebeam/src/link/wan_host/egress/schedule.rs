use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU32,
};

pub(super) const PACKETS_PER_FLOW: usize = 32;
const QUANTUM: usize = pulsebeam_link::wan::MAX_DATAGRAM_BYTES;
const CYCLE: [Class; 3] = [Class::Live, Class::Live, Class::Repair];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Class {
    Audio,
    Protected,
    Live,
    Repair,
}

impl Class {
    pub(super) const ALL: [Self; 4] = [Self::Audio, Self::Protected, Self::Live, Self::Repair];
    pub(super) const PRIORITY: [Self; 2] = [Self::Audio, Self::Protected];

    pub(super) const fn precedes(self, other: Self) -> bool {
        match self {
            Self::Audio => !matches!(other, Self::Audio),
            Self::Protected => matches!(other, Self::Live | Self::Repair),
            _ => false,
        }
    }

    pub(super) const fn capacity(self) -> usize {
        match self {
            Self::Audio => 48,
            Self::Protected => 128,
            Self::Live => 128,
            Self::Repair => 16,
        }
    }
}

pub(super) struct Schedule<T> {
    lanes: [Lane<T>; Class::ALL.len()],
    turn: usize,
}

impl<T> Default for Schedule<T> {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| Lane::default()),
            turn: 0,
        }
    }
}

struct Lane<T> {
    flows: BTreeMap<NonZeroU32, Flow<T>>,
    active: VecDeque<NonZeroU32>,
    packets: usize,
}

struct Flow<T> {
    queue: VecDeque<(usize, T)>,
    credit: usize,
}

impl<T> Default for Flow<T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            credit: 0,
        }
    }
}

impl<T> Default for Lane<T> {
    fn default() -> Self {
        Self {
            flows: BTreeMap::new(),
            active: VecDeque::new(),
            packets: 0,
        }
    }
}

impl<T> Schedule<T> {
    pub(super) fn push(&mut self, class: Class, key: NonZeroU32, bytes: usize, item: T) -> bool {
        let lane = &mut self.lanes[class as usize];
        if lane.packets >= class.capacity() || !(1..=QUANTUM).contains(&bytes) {
            return false;
        }
        let flow = lane.flows.entry(key).or_default();
        if flow.queue.len() >= PACKETS_PER_FLOW {
            return false;
        }
        if flow.queue.is_empty() {
            lane.active.push_back(key);
        }
        flow.queue.push_back((bytes, item));
        lane.packets += 1;
        true
    }

    pub(super) fn pick(&mut self, mut ready: impl FnMut(Class, bool) -> bool) -> Option<Class> {
        for class in Class::PRIORITY {
            if ready(class, self.has_packets(class)) {
                return Some(class);
            }
        }
        for offset in 0..CYCLE.len() {
            let index = (self.turn + offset) % CYCLE.len();
            let class = CYCLE[index];
            if ready(class, self.has_packets(class)) {
                self.turn = (index + 1) % CYCLE.len();
                return Some(class);
            }
        }
        None
    }

    pub(super) fn has_packets(&self, class: Class) -> bool {
        self.lanes[class as usize].packets != 0
    }

    pub(super) fn pop(&mut self, class: Class) -> Option<T> {
        let lane = &mut self.lanes[class as usize];
        let key = lane.active.pop_front()?;
        let flow = lane.flows.get_mut(&key).expect("active WAN flow");
        let (cost, item) = flow.queue.pop_front().expect("nonempty WAN flow");
        if flow.credit < cost {
            flow.credit += QUANTUM;
        }
        flow.credit -= cost;
        lane.packets -= 1;
        if let Some((next, _)) = flow.queue.front() {
            if flow.credit >= *next {
                lane.active.push_front(key);
            } else {
                lane.active.push_back(key);
            }
        } else {
            lane.flows.remove(&key);
        }
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next<T>(schedule: &mut Schedule<T>) -> Option<T> {
        let class = schedule.pick(|_, has_packets| has_packets)?;
        schedule.pop(class)
    }

    #[test]
    fn blocked_live_lane_does_not_stop_audio() {
        let mut schedule = Schedule::default();
        assert!(schedule.push(Class::Live, NonZeroU32::MIN, QUANTUM, 0));
        assert!(schedule.push(Class::Audio, NonZeroU32::MIN, 160, 1));
        assert_eq!(
            schedule.pick(|class, queued| queued && class != Class::Live),
            Some(Class::Audio)
        );
        assert_eq!(schedule.pop(Class::Audio), Some(1));
        assert_eq!(
            schedule.pick(|class, queued| queued && class != Class::Live),
            None
        );
        assert_eq!(next(&mut schedule), Some(0));
    }

    #[test]
    fn serves_equal_wire_bytes_not_equal_packet_counts() {
        let large = NonZeroU32::MIN;
        let small = large.saturating_add(1);
        let mut schedule = Schedule::default();
        for index in 0..8 {
            assert!(schedule.push(Class::Live, large, QUANTUM, (large, index)));
            assert!(schedule.push(Class::Live, small, QUANTUM / 4, (small, index)));
        }
        for turn in 0..2 {
            assert_eq!(next(&mut schedule), Some((large, turn)));
            for index in 4 * turn..4 * turn + 4 {
                assert_eq!(next(&mut schedule), Some((small, index)));
            }
        }
    }

    #[test]
    fn hot_live_flow_cannot_occupy_priority_or_repair_reserves() {
        let flow = |label| NonZeroU32::new(label).unwrap();
        let mut schedule = Schedule::default();
        for index in 0..PACKETS_PER_FLOW {
            assert!(schedule.push(Class::Live, flow(1), QUANTUM, index));
        }
        assert!(!schedule.push(Class::Live, flow(1), QUANTUM, 99));
        assert!(schedule.push(Class::Live, flow(2), QUANTUM, 100));
        assert!(schedule.push(Class::Audio, flow(3), QUANTUM, 101));
        assert!(schedule.push(Class::Repair, flow(4), QUANTUM, 102));
        assert!(schedule.push(Class::Protected, flow(5), QUANTUM, 103));
        assert_eq!(next(&mut schedule), Some(101));
        assert_eq!(next(&mut schedule), Some(103));
        assert_eq!(next(&mut schedule), Some(0));
        assert_eq!(next(&mut schedule), Some(100));
        assert_eq!(next(&mut schedule), Some(102));
        for index in 1..PACKETS_PER_FLOW {
            assert_eq!(next(&mut schedule), Some(index));
        }
        assert_eq!(next(&mut schedule), None);
    }
}
