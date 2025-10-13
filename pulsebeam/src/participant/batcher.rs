use std::{collections::VecDeque, net::SocketAddr};

/// Manages a pool of `BatcherState` objects to build GSO-compatible datagrams efficiently.
///
/// This implementation reuses memory by pooling `BatcherState` objects and is instrumented
/// with `metrics` to diagnose performance issues related to batching and allocation.
pub struct Batcher {
    cap: usize,
    active_states: VecDeque<BatcherState>,
    free_states: Vec<BatcherState>,
}

impl Batcher {
    /// Creates a new `Batcher` where each internal buffer has the specified capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            active_states: VecDeque::with_capacity(3),
            free_states: Vec::with_capacity(3),
        }
    }

    /// Returns true if there are no active batches.
    pub fn is_empty(&self) -> bool {
        self.active_states.is_empty()
    }

    /// Pushes a content slice into an appropriate batch.
    ///
    /// It attempts to find an existing batch for the same destination that is not yet sealed.
    /// If no suitable batch is found, it takes one from the free pool or allocates a new one.
    pub fn push_back(&mut self, dst: SocketAddr, content: &[u8]) {
        debug_assert!(!content.is_empty(), "Pushed content must not be empty");

        for state in &mut self.active_states {
            if state.try_push(dst, content) {
                return;
            }
        }

        let mut new_state = match self.free_states.pop() {
            Some(state) => {
                metrics::counter!("batcher_pool_status", "status" => "hit").increment(1);
                state
            }
            None => {
                metrics::counter!("batcher_pool_status", "status" => "miss").increment(1);
                BatcherState::with_capacity(self.cap)
            }
        };

        new_state.reset(dst);

        if new_state.try_push(dst, content) {
            self.active_states.push_back(new_state);
        } else {
            self.free_states.push(new_state);
            debug_assert!(
                false,
                "Content is larger than the batcher's configured capacity"
            );
        }
    }

    /// Pops a single batch from the front of the queue.
    pub fn pop_front(&mut self) -> Option<BatcherState> {
        self.active_states.pop_front()
    }

    pub fn front(&mut self) -> Option<&BatcherState> {
        self.active_states.front()
    }

    /// Reclaims a `BatcherState`, returning its memory to the pool for future reuse.
    pub fn reclaim(&mut self, state: BatcherState) {
        self.free_states.push(state);
    }
}

/// Holds the state for a single GSO-compatible batch.
pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    sealed: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(cap: usize) -> Self {
        Self {
            dst: "0.0.0.0:0".parse().unwrap(),
            segment_size: 0,
            sealed: false,
            buf: Vec::with_capacity(cap),
        }
    }

    /// Attempts to append a content slice to the buffer. Returns true on success.
    fn try_push(&mut self, dst: SocketAddr, content: &[u8]) -> bool {
        if self.sealed {
            metrics::counter!("batcher_push_rejected", "reason" => "sealed").increment(1);
            return false;
        }
        if self.dst != dst {
            return false;
        }
        if self.buf.len() + content.len() > self.buf.capacity() {
            metrics::counter!("batcher_push_rejected", "reason" => "capacity").increment(1);
            return false;
        }

        if self.segment_size == 0 {
            self.segment_size = content.len();
        }

        if content.len() == self.segment_size {
            self.buf.extend_from_slice(content);
            true
        } else {
            self.buf.extend_from_slice(content);
            self.sealed = true;
            metrics::counter!("batcher_batches_sealed_by_tail").increment(1);
            true
        }
    }

    /// Resets the state's properties for reuse.
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
    fn test_appends_same_size_and_stays_open() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4096);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(!batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2000);
    }

    #[test]
    fn test_appends_tail_and_seals() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4096);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);
        batcher.push_back(addr, &[3; 500]); // The tail packet

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2500);
    }

    #[test]
    fn test_sealed_batch_rejects_pushes_creating_new_batch() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4096);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[3; 500]); // This seals the first batch

        // A further push should be rejected and create a new batch
        batcher.push_back(addr, &[4; 1000]);
        assert_eq!(batcher.active_states.len(), 2);

        let batch1 = &batcher.active_states[0];
        let batch2 = &batcher.active_states[1];

        assert_eq!(batch1.buf.len(), 1500);
        assert!(batch1.sealed);
        assert_eq!(batch2.buf.len(), 1000);
        assert!(!batch2.sealed);
    }

    #[test]
    fn test_reclaim_and_reuse_resets_sealed_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4096);

        // Create a batch and seal it
        batcher.push_back(addr, &[1; 100]);
        batcher.push_back(addr, &[2; 50]);

        let sealed_batch = batcher.pop_front().unwrap();
        assert!(sealed_batch.sealed);
        assert!(batcher.is_empty());

        // Reclaim the sealed state
        batcher.reclaim(sealed_batch);

        // Push again, which should reuse the reclaimed state from the pool
        batcher.push_back(addr, &[3; 200]);
        assert_eq!(batcher.active_states.len(), 1);
        let reused_batch = &batcher.active_states[0];

        assert!(!reused_batch.sealed, "Reused batch should be open");
        assert_eq!(reused_batch.segment_size, 200);
        assert_eq!(reused_batch.buf.len(), 200);
    }

    #[test]
    fn test_pool_miss_allocates_new_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(1024);
        assert_eq!(batcher.free_states.len(), 0);

        // This is a pool miss
        batcher.push_back(addr, &[1; 10]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
    }

    #[test]
    fn test_pool_hit_reuses_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(1024);

        // First push causes allocation
        batcher.push_back(addr, &[1; 10]);
        let state = batcher.pop_front().unwrap();
        batcher.reclaim(state);
        assert_eq!(batcher.free_states.len(), 1);

        // Second push should be a pool hit
        batcher.push_back(addr, &[2; 20]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
    }
}
