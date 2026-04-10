use std::{cmp::Reverse, collections::BinaryHeap};

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::entity::ParticipantId;

type HeapEntry = Reverse<(Instant, ParticipantId)>;

/// A min-heap timer wheel keyed by `ParticipantId`.
///
/// Each participant tracks a single active deadline. Scheduling a new deadline
/// supersedes the previous one via lazy cancellation: stale heap entries are
/// discarded when popped rather than removed in place, keeping `schedule` O(log n).
#[derive(Default)]
pub struct TimerWheel {
    heap: BinaryHeap<HeapEntry>,
    /// Latest registered deadline per participant. Entries absent here or with
    /// a different instant than the heap entry are considered stale.
    deadlines: HashMap<ParticipantId, Instant>,
}

impl TimerWheel {
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
    pub fn schedule(&mut self, id: ParticipantId, deadline: Instant) {
        self.deadlines.insert(id, deadline);
        self.heap.push(Reverse((deadline, id)));
    }

    /// Cancel any pending deadline for `id`. Must be called when a participant
    /// is removed so heap entries referencing it are cleanly skipped.
    pub fn cancel(&mut self, id: &ParticipantId) {
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
    pub fn drain_expired(&mut self, now: Instant, mut f: impl FnMut(ParticipantId)) {
        while let Some(Reverse((t, _))) = self.heap.peek() {
            if *t > now {
                break;
            }
            let Reverse((deadline, id)) = self.heap.pop().unwrap();
            if self.deadlines.get(&id) != Some(&deadline) {
                // lazy cancelation
                continue;
            }
            self.deadlines.remove(&id);
            f(id);
        }
    }
}
