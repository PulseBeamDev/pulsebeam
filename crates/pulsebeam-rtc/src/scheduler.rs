#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    reason = "dense scheduler indexes are bounded by equal-length per-sender arrays"
)]

const QUANTUM: i64 = 1_200;

#[derive(Debug)]
pub(crate) struct SenderScheduler {
    deficits: Vec<i64>,
    cursor: usize,
}

impl SenderScheduler {
    pub(crate) fn new(senders: usize) -> Self {
        Self {
            deficits: vec![0; senders],
            cursor: 0,
        }
    }

    pub(crate) fn policy_changed(&mut self, sender: usize) {
        if let Some(deficit) = self.deficits.get_mut(sender) {
            *deficit = (*deficit).clamp(-QUANTUM, QUANTUM);
        }
    }

    pub(crate) fn select(
        &mut self,
        eligible: &[Option<usize>],
        allocations: &[u64],
    ) -> Option<usize> {
        if eligible.iter().all(Option::is_none) {
            return None;
        }
        let maximum = allocations.iter().copied().max().unwrap_or(1).max(1);
        for _ in 0..eligible.len().saturating_mul(3) {
            let index = self.cursor % eligible.len();
            let Some(bytes) = eligible[index] else {
                self.cursor = (index + 1) % eligible.len();
                continue;
            };
            let needed = i64::try_from(bytes).unwrap_or(i64::MAX);
            if self.deficits[index] >= needed {
                self.deficits[index] = self.deficits[index].saturating_sub(needed);
                if self.deficits[index] < needed {
                    self.cursor = (index + 1) % eligible.len();
                }
                return Some(index);
            }
            let quantum = u64::try_from(QUANTUM)
                .unwrap_or_default()
                .saturating_mul(allocations.get(index).copied().unwrap_or_default())
                .div_ceil(maximum)
                .max(1);
            self.deficits[index] =
                self.deficits[index].saturating_add(i64::try_from(quantum).unwrap_or(i64::MAX));
            if self.deficits[index] < needed {
                self.cursor = (index + 1) % eligible.len();
            }
        }
        eligible
            .iter()
            .enumerate()
            .filter_map(|(index, bytes)| bytes.map(|bytes| (index, bytes)))
            .min_by_key(|(index, _)| *index)
            .map(|(index, bytes)| {
                self.deficits[index] =
                    self.deficits[index].saturating_sub(i64::try_from(bytes).unwrap_or(i64::MAX));
                index
            })
    }

    pub(crate) fn commit(&mut self, _sender: usize, _payload_bytes: usize) {}
}

#[allow(
    dead_code,
    reason = "Plan 12 consumes the narrow SCTP scheduler adapter"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserLane {
    Rtp,
    Sctp,
}

#[allow(
    dead_code,
    reason = "Plan 12 consumes the narrow SCTP scheduler adapter"
)]
#[derive(Debug, Default)]
pub(crate) struct ServiceArbiter {
    next: Option<UserLane>,
}

impl ServiceArbiter {
    #[allow(
        dead_code,
        reason = "Plan 12 replaces the mock SCTP lane with association output"
    )]
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
    fn scheduler_keeps_mock_sctp_and_rtp_lanes_progressing() {
        let mut arbiter = ServiceArbiter::default();
        assert_eq!(arbiter.select(true, true), Some(UserLane::Sctp));
        assert_eq!(arbiter.select(true, true), Some(UserLane::Rtp));
        assert_eq!(arbiter.select(true, true), Some(UserLane::Sctp));
    }

    #[test]
    fn scheduler_converges_to_allocated_weight_ratio() {
        let mut scheduler = SenderScheduler::new(2);
        let mut service = [0_u64; 2];
        for _ in 0..500 {
            let sender = scheduler
                .select(&[Some(100), Some(100)], &[100, 400])
                .expect("backlogged sender");
            service[sender] += 100;
        }
        let ratio = service[1] as f64 / service[0] as f64;
        assert!((3.6..=4.4).contains(&ratio), "service={service:?}");
    }
}
