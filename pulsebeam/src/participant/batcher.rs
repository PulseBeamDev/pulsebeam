use std::{collections::VecDeque, net::SocketAddr};

pub struct Batcher {
    cap: usize,
    states: VecDeque<BatcherState>,
}

impl Batcher {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            states: VecDeque::with_capacity(3),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn push_back(&mut self, dst: SocketAddr, mut content: Vec<u8>) {
        debug_assert!(!content.is_empty());

        for state in &mut self.states {
            if let Some(bounced) = state.push(dst, content) {
                content = bounced;
            } else {
                return; // content was accepted
            }
        }

        // nobody took it — create a new state and ensure it receives the content
        let mut new_state = BatcherState::with_capacity(dst, self.cap);
        let bounced = new_state.push(dst, content);
        debug_assert!(
            bounced.is_none(),
            "Newly created state should always accept its first content"
        );
        self.states.push_back(new_state);
    }

    pub fn pop_front(&mut self) -> Option<BatcherState> {
        self.states.pop_front()
    }

    pub fn front(&self) -> Option<&BatcherState> {
        self.states.front()
    }
}

pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    is_tail: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(dst: SocketAddr, cap: usize) -> Self {
        Self {
            dst,
            segment_size: 0,
            is_tail: false,
            buf: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, dst: SocketAddr, content: Vec<u8>) -> Option<Vec<u8>> {
        debug_assert!(!content.is_empty());

        // Wrong destination or not enough capacity
        if self.dst != dst || (self.buf.len() + content.len()) > self.buf.capacity() {
            return Some(content);
        }

        if self.segment_size == 0 {
            // First segment — set the size
            self.segment_size = content.len();
        }

        let same_segment = content.len() == self.segment_size;

        if same_segment || self.is_tail {
            self.is_tail = !same_segment; // toggle tail when sizes differ
            self.buf.extend(content);
            None
        } else {
            Some(content)
        }
    }

    fn clear(&mut self) {
        self.segment_size = 0;
        self.is_tail = false;
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
    fn test_new_batcher_state() {
        let addr = create_test_addr();
        let batcher = BatcherState::with_capacity(addr, 100);

        assert_eq!(batcher.dst, addr);
        assert_eq!(batcher.segment_size, 0);
        assert_eq!(batcher.buf.len(), 0);
        assert_eq!(batcher.buf.capacity(), 100);
        assert!(!batcher.is_tail);
    }

    #[test]
    fn test_push_first_segment() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(addr, 100);

        let content = vec![1, 2, 3];
        let result = state.push(addr, content.clone());

        assert_eq!(result, None);
        assert_eq!(state.segment_size, 3);
        assert_eq!(state.buf, content);
    }

    #[test]
    fn test_push_same_segment_size() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(addr, 100);

        state.push(addr, vec![1, 2, 3]);
        let result = state.push(addr, vec![4, 5, 6]);

        assert_eq!(result, None);
        assert_eq!(state.segment_size, 3);
        assert_eq!(state.buf, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_push_different_segment_size_bounces() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(addr, 100);

        state.push(addr, vec![1, 2, 3]);
        let content = vec![4, 5];
        let bounced = state.push(addr, content.clone());

        assert_eq!(bounced, Some(content));
        assert_eq!(state.segment_size, 3);
        assert_eq!(state.buf, vec![1, 2, 3]);
    }

    #[test]
    fn test_push_different_destination_bounces() {
        let addr1 = create_test_addr();
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8080);
        let mut state = BatcherState::with_capacity(addr1, 100);

        let content = vec![1, 2, 3];
        let bounced = state.push(addr2, content.clone());

        assert_eq!(bounced, Some(content));
        assert_eq!(state.segment_size, 0);
        assert_eq!(state.buf.len(), 0);
    }

    #[test]
    fn test_push_exceeds_capacity_bounces() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(addr, 5);

        let content = vec![1, 2, 3, 4, 5, 6];
        let bounced = state.push(addr, content.clone());

        assert_eq!(bounced, Some(content));
        assert_eq!(state.segment_size, 0);
        assert!(state.buf.is_empty());
    }

    #[test]
    fn test_clear() {
        let addr = create_test_addr();
        let mut state = BatcherState::with_capacity(addr, 100);

        state.push(addr, vec![1, 2, 3]);
        state.is_tail = true;
        state.clear();

        assert_eq!(state.segment_size, 0);
        assert!(!state.is_tail);
        assert!(state.buf.is_empty());
    }

    #[test]
    fn test_batcher_push_and_pop() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(100);

        batcher.push_back(addr, vec![1, 2, 3]);

        assert!(!batcher.is_empty());
        let state = batcher.pop_front().unwrap();
        assert_eq!(state.segment_size, 3);
        assert_eq!(state.buf, vec![1, 2, 3]);
    }
}
