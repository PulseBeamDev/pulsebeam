use std::{collections::VecDeque, net::SocketAddr};

use pulsebeam_runtime::net;

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
    pub fn push_back(&mut self, dst: SocketAddr, content: &[u8]) {
        debug_assert!(!content.is_empty(), "Pushed content must not be empty");

        for state in &mut self.active_states {
            if state.try_push(dst, content) {
                return;
            }
        }

        let mut new_state = self
            .free_states
            .pop()
            .unwrap_or_else(|| BatcherState::with_capacity(self.cap));
        new_state.reset(dst);

        if new_state.try_push(dst, content) {
            self.active_states.push_back(new_state);
        } else {
            self.free_states.push(new_state);
            tracing::warn!("Content is larger than the batcher's configured capacity");
        }
    }

    /// Reclaims a `BatcherState`, returning its memory to the pool for future reuse.
    pub fn reclaim(&mut self, state: BatcherState) {
        self.free_states.push(state);
    }

    /// Flushes all pending batches to the network socket.
    pub fn flush(&mut self, socket: &net::UnifiedSocket) {
        while let Some(state) = self.active_states.front() {
            let batch = net::SendPacketBatch {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            };
            if socket.try_send_batch(&batch) {
                // The batch was sent, reclaim its state.
                if let Some(sent_state) = self.active_states.pop_front() {
                    self.reclaim(sent_state);
                }
            } else {
                // Socket is busy, stop trying to flush.
                break;
            }
        }
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

    fn try_push(&mut self, dst: SocketAddr, content: &[u8]) -> bool {
        if self.sealed || self.dst != dst || self.buf.len() + content.len() > self.buf.capacity() {
            return false;
        }

        if self.segment_size == 0 {
            self.segment_size = content.len();
        }

        if content.len() == self.segment_size {
            self.buf.extend_from_slice(content);
            true
        } else {
            // This is a "tail" packet of a different size, which seals the batch.
            self.buf.extend_from_slice(content);
            self.sealed = true;
            true
        }
    }

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
    use std::net::{IpAddr, Ipv4Addr};

    fn create_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080)
    }

    #[test]
    fn appends_same_size_and_stays_open() {
        let mut batcher = Batcher::with_capacity(4096);
        batcher.push_back(create_addr(), &[1; 1000]);
        batcher.push_back(create_addr(), &[2; 1000]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(!batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2000);
    }

    #[test]
    fn appends_tail_and_seals() {
        let mut batcher = Batcher::with_capacity(4096);
        batcher.push_back(create_addr(), &[1; 1000]);
        batcher.push_back(create_addr(), &[3; 500]);

        assert_eq!(batcher.active_states.len(), 1);
        assert!(batcher.active_states[0].sealed);
        assert_eq!(batcher.active_states[0].buf.len(), 1500);
    }
}
