use std::{
    cmp::Reverse,
    collections::{BinaryHeap, hash_map::Entry},
    hash::Hash,
};

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

type HeapEntry<T> = Reverse<(Instant, T)>;

/// A min-heap timer wheel keyed by `ParticipantId`.
///
/// Each participant tracks a single active deadline. Scheduling a new deadline
/// supersedes the previous one via lazy cancellation: stale heap entries are
/// discarded when popped rather than removed in place, keeping `schedule` O(log n).
#[derive(Default)]
pub struct TimerWheel<T: Ord + Hash + Copy> {
    heap: BinaryHeap<HeapEntry<T>>,
    /// Latest registered deadline per participant. Entries absent here or with
    /// a different instant than the heap entry are considered stale.
    deadlines: HashMap<T, Instant>,
}

impl<T: Ord + Hash + Copy> TimerWheel<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity * 2), // 1 live + 1 stale per participant
            deadlines: HashMap::with_capacity(capacity),
        }
    }

    /// Schedule (or reschedule) a deadline for `id`.
    ///
    /// If a deadline already exists for this participant it is lazily cancelled:
    /// the old heap entry stays but will be skipped on expiry.
    pub fn schedule(&mut self, id: T, deadline: Instant) {
        self.deadlines.insert(id, deadline);
        self.heap.push(Reverse((deadline, id)));
    }

    /// Cancel any pending deadline for `id`. Must be called when a participant
    /// is removed so heap entries referencing it are cleanly skipped.
    pub fn cancel(&mut self, id: &T) {
        self.deadlines.remove(id);
    }

    /// The next expiry instant, if any timers are scheduled.
    ///
    /// This is suitable for use in a `tokio::select!` sleep arm.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse((t, id))) = self.heap.peek() {
            match self.deadlines.get(id) {
                Some(current) if current == t => {
                    return Some(*t);
                }
                _ => {
                    // stale → discard
                    self.heap.pop();
                }
            }
        }
        None
    }

    /// Drain all entries whose deadline has passed as of `now`.
    ///
    /// Calls `f(id)` once per participant whose **latest** deadline has fired.
    /// Stale heap entries (superseded by a later `schedule` call) are silently
    /// discarded and `f` is **not** called for them.
    pub fn drain_expired(&mut self, now: Instant, mut f: impl FnMut(T)) {
        while let Some(Reverse((t, _))) = self.heap.peek() {
            if *t > now {
                break;
            }
            let Reverse((deadline, id)) = self.heap.pop().unwrap();

            if let Entry::Occupied(entry) = self.deadlines.entry(id)
                && *entry.get() == deadline
            {
                entry.remove();
                f(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_empty_timer_wheel() {
        let mut wheel: TimerWheel<u32> = TimerWheel::new(10);
        let now = Instant::now();

        assert_eq!(wheel.next_deadline(), None);

        let mut expired = Vec::new();
        wheel.drain_expired(now, |id| expired.push(id));
        assert!(expired.is_empty());
    }

    #[test]
    fn test_basic_schedule_and_expiry() {
        let mut wheel = TimerWheel::new(10);
        let start = Instant::now();
        let d1 = start + Duration::from_millis(100);

        wheel.schedule(1, d1);

        // Next deadline should match d1
        assert_eq!(wheel.next_deadline(), Some(d1));

        // Evaluate at a simulated time before d1 (50ms)
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(50), |id| expired.push(id));
        assert!(expired.is_empty());
        assert_eq!(wheel.next_deadline(), Some(d1));

        // Evaluate at a simulated time after d1 (110ms)
        wheel.drain_expired(start + Duration::from_millis(110), |id| expired.push(id));
        assert_eq!(expired, vec![1]);
        assert_eq!(wheel.next_deadline(), None);
    }

    #[test]
    fn test_lazy_cancellation_via_rescheduling_later() {
        let mut wheel = TimerWheel::new(10);
        let start = Instant::now();

        let d_early = start + Duration::from_millis(100);
        let d_late = start + Duration::from_millis(200);

        // Schedule early, then overwrite with a later deadline
        wheel.schedule(1, d_early);
        wheel.schedule(1, d_late);

        // next_deadline should skip the stale d_early immediately
        assert_eq!(wheel.next_deadline(), Some(d_late));

        // At 150ms (past d_early, but before d_late), nothing should fire
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(150), |id| expired.push(id));
        assert!(expired.is_empty());

        // At 250ms (past d_late), ID 1 should finally expire
        wheel.drain_expired(start + Duration::from_millis(250), |id| expired.push(id));
        assert_eq!(expired, vec![1]);
    }

    #[test]
    fn test_lazy_cancellation_via_rescheduling_earlier() {
        let mut wheel = TimerWheel::new(10);
        let start = Instant::now();

        let d_late = start + Duration::from_millis(200);
        let d_early = start + Duration::from_millis(100);

        // Schedule late, then overwrite with an earlier deadline
        wheel.schedule(1, d_late);
        wheel.schedule(1, d_early);

        // Next deadline must correctly reflect the early one
        assert_eq!(wheel.next_deadline(), Some(d_early));

        // Evaluate at 150ms (past d_early), ID 1 fires
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(150), |id| expired.push(id));
        assert_eq!(expired, vec![1]);

        // Evaluate at 250ms (past the original late deadline), nothing else fires
        let mut expired_again = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(250), |id| {
            expired_again.push(id)
        });
        assert!(expired_again.is_empty());
    }

    #[test]
    fn test_explicit_cancel() {
        let mut wheel = TimerWheel::new(10);
        let start = Instant::now();
        let d1 = start + Duration::from_millis(100);

        wheel.schedule(1, d1);
        assert_eq!(wheel.next_deadline(), Some(d1));

        // Explicitly cancel ID 1
        wheel.cancel(&1);

        // Next deadline should automatically clear the canceled target
        assert_eq!(wheel.next_deadline(), None);

        // Even passing the original deadline timestamp should yield no fires
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(150), |id| expired.push(id));
        assert!(expired.is_empty());
    }

    #[test]
    fn test_multiple_participants_ordering() {
        let mut wheel = TimerWheel::new(10);
        let start = Instant::now();

        // Schedule deadlines out of chronological order
        wheel.schedule(2, start + Duration::from_millis(200));
        wheel.schedule(1, start + Duration::from_millis(100));
        wheel.schedule(3, start + Duration::from_millis(300));

        // Next deadline must prioritize the earliest active item (ID 1)
        assert_eq!(
            wheel.next_deadline(),
            Some(start + Duration::from_millis(100))
        );

        // Drain at 250ms -> fires 1 and 2, leaving 3
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(250), |id| expired.push(id));
        assert_eq!(expired, vec![1, 2]);
        assert_eq!(
            wheel.next_deadline(),
            Some(start + Duration::from_millis(300))
        );

        // Drain at 350ms -> fires 3
        let mut expired_remaining = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(350), |id| {
            expired_remaining.push(id)
        });
        assert_eq!(expired_remaining, vec![3]);
    }

    #[test]
    fn test_massive_rescheduling_churn() {
        let mut wheel = TimerWheel::new(5);
        let start = Instant::now();

        // Continually push new deadlines for ID 1
        for offset in (10..=1000).step_by(10) {
            wheel.schedule(1, start + Duration::from_millis(offset));
        }

        // The deadline must successfully settle on the latest schedule (1000ms)
        assert_eq!(
            wheel.next_deadline(),
            Some(start + Duration::from_millis(1000))
        );

        // At 999ms, the internal heap entries are drained as stale, but no callback is triggered
        let mut expired = Vec::new();
        wheel.drain_expired(start + Duration::from_millis(999), |id| expired.push(id));
        assert!(expired.is_empty());

        // At 1001ms, the actual deadline triggers
        wheel.drain_expired(start + Duration::from_millis(1001), |id| expired.push(id));
        assert_eq!(expired, vec![1]);
    }
}
