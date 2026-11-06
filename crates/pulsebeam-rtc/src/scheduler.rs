use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};

const QUANTUM: i64 = 1_200;
const WORK_BUDGET: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct SenderScheduler {
    deficits: BTreeMap<u64, i64>,
    cursor: u64,
    deferred: bool,
}

impl SenderScheduler {
    pub(crate) const fn sender_payload_bytes() -> usize {
        std::mem::size_of::<(u64, i64)>()
    }

    pub(crate) fn retire(&mut self, sender: u64) {
        self.deficits.remove(&sender);
    }

    pub(crate) const fn deferred(&self) -> bool {
        self.deferred
    }

    pub(crate) fn candidate(&self, eligible: &BTreeMap<u64, usize>) -> Option<u64> {
        eligible
            .range(self.cursor..)
            .next()
            .or_else(|| eligible.first_key_value())
            .map(|(&sender, _)| sender)
    }

    pub(crate) fn select(&mut self, eligible: &BTreeMap<u64, usize>) -> Option<u64> {
        self.deferred = false;
        for _ in 0..WORK_BUDGET {
            let (&sender, &bytes) = eligible
                .range(self.cursor..)
                .next()
                .or_else(|| eligible.first_key_value())?;
            let next = eligible
                .range((Excluded(sender), Unbounded))
                .next()
                .or_else(|| eligible.first_key_value())
                .map_or(sender, |(&key, _)| key);
            let needed = i64::try_from(bytes).unwrap_or(i64::MAX);
            let deficit = self.deficits.entry(sender).or_default();
            if *deficit >= needed {
                *deficit = deficit.saturating_sub(needed);
                self.cursor = if *deficit >= needed { sender } else { next };
                return Some(sender);
            }
            *deficit = deficit.saturating_add(QUANTUM);
            self.cursor = if *deficit >= needed { sender } else { next };
        }
        self.deferred = !eligible.is_empty();
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserLane {
    Rtp,
    Sctp,
}

#[derive(Debug, Default)]
pub(crate) struct ServiceArbiter {
    next: Option<UserLane>,
}

impl ServiceArbiter {
    pub(crate) fn select(&mut self, rtp: bool, sctp: bool) -> Option<UserLane> {
        let selected = match (rtp, sctp, self.next) {
            (false, false, _) => None,
            (true, false, _) | (true, true, Some(UserLane::Rtp)) => Some(UserLane::Rtp),
            (false, true, _) | (true, true, _) => Some(UserLane::Sctp),
        };
        self.next = selected.map(|lane| match lane {
            UserLane::Rtp => UserLane::Sctp,
            UserLane::Sctp => UserLane::Rtp,
        });
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_keeps_sctp_and_rtp_lanes_progressing() {
        let mut arbiter = ServiceArbiter::default();
        assert_eq!(arbiter.select(true, true), Some(UserLane::Sctp));
        assert_eq!(arbiter.select(true, true), Some(UserLane::Rtp));
        assert_eq!(arbiter.select(true, true), Some(UserLane::Sctp));
    }

    #[test]
    fn scheduler_serves_backlogged_senders_without_target_weights() {
        let mut scheduler = SenderScheduler::default();
        let mut service = (0_u64, 0_u64);
        let eligible = BTreeMap::from([(0, 100), (1, 1_000)]);
        for _ in 0..500 {
            let sender = scheduler.select(&eligible).expect("backlogged sender");
            match sender {
                0 => service.0 = service.0.saturating_add(100),
                1 => service.1 = service.1.saturating_add(1_000),
                _ => panic!("unknown sender {sender}"),
            }
        }
        assert!(
            service.0.abs_diff(service.1) <= u64::try_from(QUANTUM.saturating_mul(2)).unwrap(),
            "service={service:?}"
        );
        assert_eq!(scheduler.select(&BTreeMap::new()), None);
        assert_eq!(scheduler.select(&BTreeMap::from([(1, 100)])), Some(1));
        scheduler.retire(1);
        assert!(!scheduler.deficits.contains_key(&1));
        let sparse = BTreeMap::from([(65_537, 65_535), (9_000_000, 65_535)]);
        let mut selected = BTreeMap::<u64, usize>::new();
        for _ in 0..200 {
            if let Some(sender) = scheduler.select(&sparse) {
                let count = selected.entry(sender).or_default();
                *count = count.saturating_add(1);
            }
        }
        assert!(selected.get(&65_537).is_some_and(|count| *count > 0));
        assert!(selected.get(&9_000_000).is_some_and(|count| *count > 0));
        let mut scheduler = SenderScheduler::default();
        let backlog = (0..257_u64).map(|sender| (sender, 2_000)).collect();
        assert_eq!(scheduler.select(&backlog), None);
        assert!(scheduler.deferred());
        assert!(scheduler.select(&backlog).is_some());
    }
}
