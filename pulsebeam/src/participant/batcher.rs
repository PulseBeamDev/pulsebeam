use pulsebeam_runtime::net;
use std::{collections::VecDeque, net::SocketAddr};

const MAX_FREE_STATES: usize = 3;

/// Manages a pool of `BatcherState` objects to build GSO-compatible datagrams efficiently.
pub struct Batcher {
    cap: usize,
    active_states: VecDeque<BatcherState>,
    free_states: Vec<BatcherState>,
}

impl Batcher {
    /// Creates a new `Batcher` where each internal buffer has the specified capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            // The socket reports the kernel's actual UDP_SEGMENT fan-out
            // limit. Do not silently reduce it here: doing so turns a 64
            // segment GSO-capable socket into eight-datagram submissions.
            cap,
            active_states: VecDeque::with_capacity(3),
            free_states: Vec::with_capacity(MAX_FREE_STATES),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.active_states.is_empty()
    }

    /// Pushes a content slice into an appropriate batch.
    ///
    /// It attempts to find an existing batch for the same destination that is not yet sealed.
    /// If no suitable batch is found, it takes one from the free pool or allocates a new one.
    pub fn push_back(&mut self, dst: SocketAddr, content: &[u8]) {
        debug_assert!(!content.is_empty(), "Pushed content must not be empty");

        if let Some(state) = self.active_states.back_mut()
            && state.try_push(dst, content)
        {
            return;
        }

        let mut new_state = match self.free_states.pop() {
            Some(state) => state,
            None => BatcherState::with_capacity(self.cap),
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
        if self.free_states.len() < self.free_states.capacity() {
            self.free_states.push(state);
        }
    }

    /// Exposes every completed GSO datagram without copying its payload.
    /// The shard gathers these from all dirty participants into one
    /// `sendmmsg()` submission, then calls `discard_all()` after the
    /// lossy egress decision has been made.
    pub fn packets(&self) -> impl Iterator<Item = net::SendPacket<'_>> + '_ {
        self.active_states.iter().map(|state| net::SendPacket {
            dst: state.dst,
            buf: &state.buf,
            segment_size: state.segment_size,
        })
    }

    /// Releases every queued packet after the output phase.  UDP/TCP egress
    /// is deliberately lossy, so a short send or `WouldBlock` also drains the
    /// queue instead of retaining latency-inducing backlog.
    pub fn discard_all(&mut self) {
        while let Some(state) = self.pop_front() {
            self.reclaim(state);
        }
    }

    pub fn flush_tcp(&mut self, socket: &mut net::tcp::TcpTransport) {
        while let Some(state) = self.front() {
            debug_assert!(state.segment_count > 0, "Attempted to flush an empty batch");
            debug_assert!(
                state.segment_size != 0,
                "BatcherState must have a nonzero segment_size before flush"
            );
            debug_assert!(
                state.buf.len() <= state.max_segments * net::MAX_UDP_PAYLOAD_SIZE,
                "Batch exceeds configured TCP batch capacity"
            );
            let packet = [net::SendPacket {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            }];
            let res = socket.try_send_batch(&net::SendPacketBatch { packets: &packet });
            match res {
                Ok(_) => {
                    let state = self.pop_front().unwrap();
                    self.reclaim(state);
                }
                Err(err) => {
                    tracing::trace!("error on writing to TCP socket: {:?}", err);
                    let state = self.pop_front().unwrap();
                    self.reclaim(state);
                }
            }
        }
    }
}

/// Holds the state for a single GSO-compatible batch.
pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    segment_count: usize,
    max_segments: usize,
    sealed: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(cap: usize) -> Self {
        debug_assert_ne!(cap, 0);
        Self {
            dst: "0.0.0.0:0".parse().unwrap(),
            segment_size: 0,
            segment_count: 0,
            max_segments: cap,
            sealed: false,
            // Reserving the full GSO maximum for every participant is costly
            // at SFU scale. Grow only for the batches that are actually used;
            // recycled states retain that capacity for the next tick.
            buf: Vec::with_capacity(cap * net::MAX_UDP_PAYLOAD_SIZE),
        }
    }

    /// Attempts to append a content slice to the buffer. Returns true on success.
    fn try_push(&mut self, dst: SocketAddr, content: &[u8]) -> bool {
        debug_assert!(!content.is_empty(), "Segment content must not be empty");
        debug_assert!(
            content.len() <= net::MAX_UDP_PAYLOAD_SIZE,
            "Segment content exceeds maximum supported MTU"
        );
        debug_assert_eq!(self.buf.is_empty(), self.segment_count == 0);
        debug_assert_eq!(self.segment_size == 0, self.segment_count == 0);
        debug_assert!(self.segment_count <= self.max_segments);
        debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);

        if self.sealed {
            return false;
        }

        if self.dst != dst {
            return false;
        }

        if self.segment_count >= self.max_segments {
            return false;
        }

        if self.buf.len() + content.len() > net::MAX_UDP_GSO_PAYLOAD_SIZE {
            return false;
        }

        if self.segment_size == 0 {
            self.segment_size = content.len();
        }

        if content.len() == self.segment_size {
            debug_assert!(
                self.buf.len() + content.len() <= self.max_segments * net::MAX_UDP_PAYLOAD_SIZE
            );
            self.buf.extend_from_slice(content);
            self.segment_count += 1;
            debug_assert!(self.segment_count <= self.max_segments);
            debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
            true
        } else if content.len() < self.segment_size {
            debug_assert!(
                self.buf.len() + content.len() <= self.max_segments * net::MAX_UDP_PAYLOAD_SIZE
            );
            self.buf.extend_from_slice(content);
            self.segment_count += 1;
            self.sealed = true;
            debug_assert!(self.segment_count <= self.max_segments);
            debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
            true
        } else {
            false
        }
    }

    /// Resets the state's properties for reuse.
    fn reset(&mut self, dst: SocketAddr) {
        self.dst = dst;
        self.segment_size = 0;
        self.segment_count = 0;
        self.sealed = false;
        self.buf.clear();
        debug_assert_eq!(self.segment_size, 0);
        debug_assert_eq!(self.segment_count, 0);
        debug_assert!(self.buf.is_empty());
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
        let mut batcher = Batcher::with_capacity(4);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(!batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2000);
    }

    #[test]
    fn test_uses_the_socket_reported_gso_capacity() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(16);

        for _ in 0..16 {
            batcher.push_back(addr, &[1; 1000]);
        }

        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.active_states[0].segment_count, 16);
        assert_eq!(batcher.active_states[0].max_segments, 16);
    }

    #[test]
    fn test_gso_payload_limit_seals_before_segment_limit() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(64);
        let segment = [0; net::MAX_UDP_PAYLOAD_SIZE];
        let count = net::MAX_UDP_GSO_PAYLOAD_SIZE / segment.len();

        for _ in 0..=count {
            batcher.push_back(addr, &segment);
        }

        assert_eq!(batcher.active_states.len(), 2);
        assert_eq!(batcher.active_states[0].segment_count, count);
        assert_eq!(batcher.active_states[1].segment_count, 1);
    }

    #[test]
    fn test_gso_payload_limit_scales_with_segment_size() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(64);
        let segment = [0; 1200];
        let count = net::MAX_UDP_GSO_PAYLOAD_SIZE / segment.len();

        for _ in 0..=count {
            batcher.push_back(addr, &segment);
        }

        assert_eq!(batcher.active_states[0].segment_count, count);
        assert_eq!(batcher.active_states[1].segment_count, 1);
    }

    #[test]
    fn test_appends_tail_and_seals() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);

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
        let mut batcher = Batcher::with_capacity(4);

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
        let mut batcher = Batcher::with_capacity(4);

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
        let mut batcher = Batcher::with_capacity(4);
        assert_eq!(batcher.free_states.len(), 0);

        // This is a pool miss
        batcher.push_back(addr, &[1; 10]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
    }

    #[test]
    fn test_pool_hit_reuses_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(1);

        // First push causes allocation
        batcher.push_back(addr, &[1; 10]);
        let state = batcher.pop_front().unwrap();
        batcher.reclaim(state);
        assert_eq!(batcher.free_states.len(), 1);

        // Second push should be a pool hit
        batcher.push_back(addr, &[2; 20]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
        let state = batcher.pop_front().unwrap();
        assert_eq!(state.buf, [2; 20]);
        assert_eq!(state.segment_size, 20);
        assert_eq!(state.dst, addr);
        batcher.reclaim(state);

        // Third shrinks the content
        batcher.push_back(addr, &[3; 5]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
        let state = batcher.pop_front().unwrap();
        assert_eq!(state.buf, [3; 5]);
        assert_eq!(state.segment_size, 5);
        assert_eq!(state.dst, addr);
        batcher.reclaim(state);
    }

    #[test]
    fn test_seal_unequal_size() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(3);

        // First push causes allocation
        batcher.push_back(addr, &[1; 10]);
        batcher.push_back(addr, &[2; 10]);
        // This is larger than last segment, it shouldn't be allowed
        batcher.push_back(addr, &[3; 11]);
        batcher.push_back(addr, &[4; 11]);
        batcher.push_back(addr, &[5; 11]);
        assert_eq!(batcher.active_states.len(), 2);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 20);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 33);

        batcher.push_back(addr, &[1; 10]);
        batcher.push_back(addr, &[2; 10]);
        batcher.push_back(addr, &[3; 9]);
        batcher.push_back(addr, &[4; 9]);
        batcher.push_back(addr, &[5; 9]);
        assert_eq!(batcher.active_states.len(), 2);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 29);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 18);
    }

    #[test]
    fn test_segment_capacity_limits_and_batch_creation() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(2);

        batcher.push_back(addr, &[1; 500]);
        batcher.push_back(addr, &[2; 500]);
        batcher.push_back(addr, &[3; 500]);

        assert_eq!(batcher.active_states.len(), 2);
        let first = batcher.pop_front().unwrap();
        assert_eq!(first.segment_count, 2);
        assert_eq!(first.buf.len(), 1000);
        assert!(!first.sealed);

        let second = batcher.pop_front().unwrap();
        assert_eq!(second.segment_count, 1);
        assert_eq!(second.buf.len(), 500);
        assert_eq!(second.segment_size, 500);
    }

    #[test]
    fn test_batcher_seals_on_smaller_tail_packet() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(3);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);
        batcher.push_back(addr, &[3; 500]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.segment_count, 3);
        assert_eq!(batch.buf.len(), 2500);
    }
}
