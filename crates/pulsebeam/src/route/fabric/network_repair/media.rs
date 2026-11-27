use std::{collections::VecDeque, num::NonZeroU64};

use pulsebeam_core::fabric::wan::OmissionRun;
use pulsebeam_runtime::time::{Duration, Instant};

use super::{super::LiveNetworkChild, MAX_REPAIR_LIFETIME};

const REORDER_GRACE: Duration = Duration::from_millis(25);
const GAP_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MediaInterest {
    pub(crate) parent: LiveNetworkChild,
    pub(crate) branch: NonZeroU64,
    pub(crate) position: u64,
    pub(crate) expires: Instant,
}

#[derive(Default)]
pub(crate) struct MediaGaps {
    highest: Option<u64>,
    pending: VecDeque<Gap>,
}

struct Gap {
    position: u64,
    due: Instant,
    expires: Instant,
}

impl MediaGaps {
    /// Turn a small hole in one installed receive window into an exact request
    /// hint. This does not declare transport loss or stall media forwarding.
    pub(crate) fn observe(
        &mut self,
        position: u64,
        omitted: Option<OmissionRun>,
        now: Instant,
        mut already_admitted: impl FnMut(u64) -> bool,
    ) -> Option<(u64, Instant)> {
        let skipped = omitted.and_then(|run| run.positions(position));
        self.pending.retain(|gap| {
            gap.position != position
                && now < gap.expires
                && skipped
                    .as_ref()
                    .is_none_or(|range| !range.contains(&gap.position))
        });
        if self.highest.is_none_or(|highest| position > highest) {
            let previous = self.highest.replace(position);
            if let Some(start) = previous.and_then(|highest| highest.checked_add(1)) {
                let end = skipped.as_ref().map_or(position, |range| range.start);
                let missing = end.saturating_sub(start);
                if missing > GAP_LIMIT as u64 {
                    self.pending.clear();
                } else if missing != 0 {
                    let due = now.checked_add(REORDER_GRACE)?;
                    let expires = now.checked_add(MAX_REPAIR_LIFETIME)?;
                    let room = GAP_LIMIT.saturating_sub(self.pending.len());
                    self.pending.extend(
                        (start..end)
                            .filter(|&missing| !already_admitted(missing))
                            .take(room)
                            .map(|position| Gap {
                                position,
                                due,
                                expires,
                            }),
                    );
                }
            }
        }
        while self.pending.front().is_some_and(|gap| now >= gap.due) {
            let gap = self.pending.pop_front()?;
            if !already_admitted(gap.position) {
                return Some((gap.position, gap.expires));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_gap_hint_tolerates_reordering_and_other_branches_without_reopening() {
        let now = Instant::now();
        let mut gaps = MediaGaps::default();
        assert_eq!(gaps.observe(0x18000, None, now, |_| false), None);
        assert_eq!(gaps.observe(0x18004, None, now, |_| false), None);
        assert_eq!(gaps.observe(0x18001, None, now, |_| false), None);
        assert_eq!(
            gaps.observe(0x18005, None, now + REORDER_GRACE, |position| position
                == 0x18002),
            Some((0x18003, now + MAX_REPAIR_LIFETIME))
        );
        assert_eq!(
            gaps.observe(0x18006, None, now + REORDER_GRACE, |_| false),
            None
        );
        assert_eq!(
            gaps.observe(0x1ffff, None, now + REORDER_GRACE, |_| false),
            None
        );
        assert_eq!(
            gaps.observe(0x1ffff, None, now + Duration::from_secs(1), |_| false),
            None
        );
    }

    #[test]
    fn only_actual_holes_are_requested_after_an_omission_run() {
        let now = Instant::now();
        let mut gaps = MediaGaps::default();
        assert_eq!(gaps.observe(100, None, now, |_| false), None);
        assert_eq!(gaps.observe(104, None, now, |_| false), None);
        assert_eq!(gaps.observe(103, OmissionRun::new(1), now, |_| false), None);
        assert_eq!(
            gaps.observe(108, OmissionRun::new(2), now + REORDER_GRACE, |_| false),
            Some((101, now + MAX_REPAIR_LIFETIME))
        );
        assert_eq!(
            gaps.observe(109, None, now + REORDER_GRACE * 2, |_| false),
            Some((105, now + REORDER_GRACE + MAX_REPAIR_LIFETIME))
        );
        assert_eq!(
            gaps.observe(110, None, now + REORDER_GRACE * 2, |_| false),
            None
        );
    }
}
