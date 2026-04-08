use std::{cmp::Reverse, collections::BinaryHeap};

use ahash::HashMap;
use tokio::time::Instant;

use crate::entity::ParticipantKey;

type HeapEntry = Reverse<(Instant, ParticipantKey)>;

/// A min-heap timer wheel keyed by `ParticipantKey`.
///
/// Each participant tracks a single active deadline. Scheduling a new deadline
/// supersedes the previous one via lazy cancellation: stale heap entries are
/// discarded when popped rather than removed in place, keeping `schedule` O(log n).
#[derive(Default)]
pub struct TimerWheel {
    heap: BinaryHeap<HeapEntry>,
    /// Latest registered deadline per participant. Entries absent here or with
    /// a different instant than the heap entry are considered stale.
    deadlines: HashMap<ParticipantKey, Instant>,
}

impl TimerWheel {
    /// Schedule (or reschedule) a deadline for `id`.
    ///
    /// If a deadline already exists for this participant it is lazily cancelled:
    /// the old heap entry stays but will be skipped on expiry.
    pub fn schedule(&mut self, id: ParticipantKey, deadline: Instant) {
        self.deadlines.insert(id, deadline);
        self.heap.push(Reverse((deadline, id)));
    }

    /// Cancel any pending deadline for `id`. Must be called when a participant
    /// is removed so heap entries referencing it are cleanly skipped.
    pub fn cancel(&mut self, id: ParticipantKey) {
        self.deadlines.remove(&id);
    }

    /// The next expiry instant, if any timers are scheduled.
    ///
    /// This is suitable for use in a `tokio::select!` sleep arm.
    pub fn next_deadline(&self) -> Option<Instant> {
        // Peek may return a stale entry, but its instant is always >= the real
        // next deadline, so we might sleep a little longer — never shorter.
        // The expiry loop below will then immediately drain all due entries.
        self.heap.peek().map(|Reverse((t, _))| *t)
    }

    /// Drain all entries whose deadline has passed as of `now`.
    ///
    /// Calls `f(id)` once per participant whose **latest** deadline has fired.
    /// Stale heap entries (superseded by a later `schedule` call) are silently
    /// discarded and `f` is **not** called for them.
    pub fn drain_expired(&mut self, now: Instant, mut f: impl FnMut(ParticipantKey)) {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if deadline > now {
                break;
            }
            self.heap.pop();

            // Skip stale: a newer deadline was registered after this heap entry.
            if self.deadlines.get(&id) != Some(&deadline) {
                continue;
            }

            // Entry is live and expired — remove tracking and fire.
            self.deadlines.remove(&id);
            f(id);
        }
    }
}
