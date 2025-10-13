use std::{collections::VecDeque, net::SocketAddr};

/// Manages a pool of BatcherStates to build and emit network packets efficiently.
/// This implementation reuses memory by pooling BatcherState objects,
/// avoiding allocations on the hot path after an initial warm-up.
pub struct Batcher {
    cap: usize,
    active_states: VecDeque<BatcherState>,
    free_states: Vec<BatcherState>, // Pool of states for memory reuse
}

impl Batcher {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            active_states: VecDeque::with_capacity(3),
            free_states: Vec::with_capacity(3),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active_states.is_empty()
    }

    /// Pushes content to an appropriate batch.
    /// This operation is zero-allocation if a reusable state is available in the pool.
    pub fn push_back(&mut self, dst: SocketAddr, content: &[u8]) {
        debug_assert!(!content.is_empty(), "Content should not be empty");

        // Attempt to push to an existing active state
        for state in &mut self.active_states {
            if state.try_push(dst, content) {
                return;
            }
        }

        // Otherwise, grab a state from the free pool or create a new one
        let mut new_state = self.free_states.pop().unwrap_or_else(|| {
            // Allocation only happens here when the pool is empty
            BatcherState::with_capacity(self.cap)
        });

        new_state.reset(dst);

        // A new state should always accept the first content push.
        // The debug_assert ensures we catch cases where content > capacity.
        if new_state.try_push(dst, content) {
            self.active_states.push_back(new_state);
        } else {
            // This path should only be taken if the initial content is larger
            // than the batcher's capacity. We put the state back.
            self.free_states.push(new_state);
            debug_assert!(false, "Content is larger than the batcher capacity");
        }
    }

    /// Returns a completed batch for processing.
    /// The caller is responsible for returning it via `reclaim()` to prevent memory leaks.
    pub fn pop_front(&mut self) -> Option<BatcherState> {
        self.active_states.pop_front()
    }

    /// Reclaims a BatcherState, returning its memory to the pool for reuse.
    pub fn reclaim(&mut self, mut state: BatcherState) {
        // We could clear the buffer here, but `reset` already does it upon reuse,
        // which is slightly more efficient as it avoids a clear on the final use.
        self.free_states.push(state);
    }

    pub fn front(&self) -> Option<&BatcherState> {
        self.active_states.front()
    }
}

pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    sealed: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(cap: usize) -> Self {
        Self {
            dst: "0.0.0.0:0".parse().unwrap(), // Default, overwritten on reset
            segment_size: 0,
            sealed: false,
            buf: Vec::with_capacity(cap),
        }
    }

    /// Tries to append content to the buffer. Returns true on success.
    fn try_push(&mut self, dst: SocketAddr, content: &[u8]) -> bool {
        // A sealed batch cannot accept more data.
        if self.sealed || self.dst != dst || self.buf.len() + content.len() > self.buf.capacity() {
            return false;
        }

        // First segment pushed to this state, establish the segment size.
        if self.segment_size == 0 {
            self.segment_size = content.len();
        }

        if content.len() == self.segment_size {
            // Segment has the expected size, append it.
            self.buf.extend_from_slice(content);
            true
        } else {
            // This is a different-sized "tail" segment.
            // Append it and seal the batch from further writes.
            self.buf.extend_from_slice(content);
            self.sealed = true;
            true
        }
    }

    /// Resets the state for reuse.
    fn reset(&mut self, dst: SocketAddr) {
        self.dst = dst;
        self.segment_size = 0;
        self.sealed = false;
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn create_test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080)
    }

    #[test]
    fn test_push_same_size_segments() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(100);
        state.reset(addr);

        assert!(state.try_push(addr, &[1, 2, 3]));
        assert_eq!(state.segment_size, 3);
        assert!(!state.sealed);

        assert!(state.try_push(addr, &[4, 5, 6]));
        assert_eq!(state.buf, &[1, 2, 3, 4, 5, 6]);
        assert!(!state.sealed); // Not sealed after same-size push
    }

    #[test]
    fn test_push_tail_segment_seals_state() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(100);
        state.reset(addr);

        state.try_push(addr, &[1, 2, 3]);
        assert!(state.try_push(addr, &[4, 5])); // Push a tail segment

        assert!(state.sealed); // State should now be sealed
        assert_eq!(state.buf, &[1, 2, 3, 4, 5]);

        // Further pushes should be rejected
        assert!(!state.try_push(addr, &[6, 7, 8]));
        assert!(!state.try_push(addr, &[9, 10]));

        // Buffer should remain unchanged
        assert_eq!(state.buf, &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_rejects_push_when_sealed() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(100);
        state.reset(addr);
        state.sealed = true; // Manually seal for test

        assert!(!state.try_push(addr, &[1, 2, 3]));
        assert!(state.buf.is_empty());
    }

    #[test]
    fn test_reclaim_and_reuse_resets_sealed_flag() {
        let addr1 = create_test_addr();
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8080);
        let mut batcher = Batcher::with_capacity(100);

        // Create a state, push a tail, which seals it
        batcher.push_back(addr1, &[1, 2, 3]);
        batcher.push_back(addr1, &[4, 5]);

        let mut state = batcher.pop_front().unwrap();
        assert!(state.sealed);

        // Reclaim the sealed state
        batcher.reclaim(state);

        // Reuse the state for a new destination
        batcher.push_back(addr2, &[10, 20]);
        let reused_state = batcher.front().unwrap();

        assert_eq!(reused_state.dst, addr2);
        assert!(!reused_state.sealed, "Reused state should not be sealed");
        assert_eq!(reused_state.buf, &[10, 20]);
    }

    #[test]
    fn test_batcher_finds_correct_active_state() {
        let addr1 = create_test_addr();
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8080);
        let mut batcher = Batcher::with_capacity(100);

        batcher.push_back(addr1, &[1; 10]);
        batcher.push_back(addr2, &[2; 10]);

        assert_eq!(batcher.active_states.len(), 2);

        // This push should find the existing state for addr2, not addr1
        batcher.push_back(addr2, &[3; 10]);
        assert_eq!(batcher.active_states.len(), 2);

        let s1 = batcher.pop_front().unwrap();
        assert_eq!(s1.dst, addr1);
        assert_eq!(s1.buf.len(), 10);
        batcher.reclaim(s1);

        let s2 = batcher.pop_front().unwrap();
        assert_eq!(s2.dst, addr2);
        assert_eq!(s2.buf.len(), 20); // Both pushes for addr2 landed here
        batcher.reclaim(s2);
    }
}
